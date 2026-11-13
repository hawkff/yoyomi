//! Text under the mouse on PDF pages and laid-out text pages: a drag selects
//! text in reading order and copies it on release, a click follows a link,
//! hovering names where a link goes and a right click copies it.
//!
//! Text layers load on job threads the first time the mouse moves over or
//! presses a page, so the input loop never waits for pdftotext. An action on
//! a page whose layer is still loading runs once it arrives, unless newer
//! input came first.

mod clipboard;
mod text;
mod url;

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::Sender;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use yoyomi_core::{Book, Cancel, Grid, Jobs, Layer, Link, PageCache, PageKind, PageRect, Priority, Result, TextLayer};
use yoyomi_view::Shown;

use self::clipboard::{COPIES, Session, open_url, osc52};
use self::text::{link_at, select};
use self::url::clean_url;
use crate::app::{Event, ImageBox};

/// How many page layers the reader keeps beside the book's own cache.
const MAX_LAYERS: usize = 64;

/// A page with selectable text and the screen cells that show it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Slot {
    pub page: usize,
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl Slot {
    fn contains(&self, x: u16, y: u16) -> bool {
        (self.x..self.x + self.cols).contains(&x) && (self.y..self.y + self.rows).contains(&y)
    }
}

/// The pages under the mouse and the zoom crop they show.
pub(crate) struct View {
    pub slots: Vec<Slot>,
    pub zoom: f64,
    pub center: (f64, f64),
}

impl View {
    /// The point of the page under cell (`x`, `y`).
    fn hit(&self, x: u16, y: u16) -> Option<Hit> {
        let slot = *self.slots.iter().find(|slot| slot.contains(x, y))?;
        Some(self.hit_in(slot, x, y))
    }

    /// The point of the page in `slot` under cell (`x`, `y`); cells outside
    /// it move to its edge, so a drag can run past the page.
    fn hit_in(&self, slot: Slot, x: u16, y: u16) -> Hit {
        let x = x.clamp(slot.x, slot.x + slot.cols - 1);
        let y = y.clamp(slot.y, slot.y + slot.rows - 1);
        let fraction = |cell: u16, start: u16, cells: u16| (f64::from(cell - start) + 0.5) / f64::from(cells);
        // Undo the zoom crop, which shows 1 / zoom of each side around the center.
        let view = 1.0 / self.zoom.max(1.0);
        let unzoom = |f: f64, center: f64| (center - view / 2.0).max(0.0).min(1.0 - view) + f * view;
        Hit { slot, x: unzoom(fraction(x, slot.x, slot.cols), self.center.0), y: unzoom(fraction(y, slot.y, slot.rows), self.center.1) }
    }

    fn shows(&self, page: usize) -> bool {
        self.slots.iter().any(|slot| slot.page == page)
    }
}

/// The pages of `shown` that `selectable` accepts, where the image box puts
/// them on a screen of `screen_rows` rows; a sixel image the draw leaves out
/// has none. A frame that starts right of the middle shows the second page
/// of a spread; a single page sits centered and starts left of it.
pub(crate) fn slots(shown: &Shown, image: ImageBox, screen_rows: u16, selectable: impl Fn(usize) -> bool) -> Vec<Slot> {
    let half = image.cols.saturating_sub(1) / 2;
    let mut slots = Vec::new();
    for layer in shown.scene.layers.iter().filter(|layer| image.shows(layer, screen_rows)) {
        let (at, grid) = match layer {
            Layer::KittyRows { cols, rows, at, .. } => (*at, Grid::new(*cols, *rows)),
            Layer::Sixel { grid, at, .. } | Layer::Cells { grid, at, .. } => (*at, *grid),
            _ => continue,
        };
        let page = shown.page + usize::from(at.col > half);
        let cols = grid.cols.min(image.cols.saturating_sub(at.col));
        let rows = grid.rows.min(image.rows.saturating_sub(at.row));
        if cols > 0 && rows > 0 && selectable(page) {
            slots.push(Slot { page, x: image.x + at.col, y: image.y + at.row, cols, rows });
        }
    }
    slots
}

/// A point on a page, in page fractions.
#[derive(Clone, Copy, Debug)]
struct Hit {
    slot: Slot,
    x: f64,
    y: f64,
}

/// A selection between two points of one page.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Selection {
    page: usize,
    from: (f64, f64),
    to: (f64, f64),
}

/// A click, a selection or a right click that waits for its page's layer.
#[derive(Clone, Copy, Debug)]
struct Action {
    hit: Hit,
    selection: Option<Selection>,
    copy_link: bool,
    /// The input count when the action came.
    input: u64,
}

/// What the reader does after mouse input or a loaded layer.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Outcome {
    /// The new status message; an empty one clears it.
    pub status: Option<String>,
    /// The highlights changed, so the page renders again.
    pub redraw: bool,
    /// A link leads to this page.
    pub go_to: Option<usize>,
}

/// The selection state of one open book.
pub(crate) struct Select {
    book: Arc<PageCache>,
    jobs: Jobs,
    events: Sender<Event>,
    /// Loaded layers; `None` for pages without text.
    layers: HashMap<usize, Option<Arc<TextLayer>>>,
    loading: HashMap<usize, Cancel>,
    /// Where the left button went down, and the page point there.
    press: Option<(u16, u16)>,
    drag: Option<Hit>,
    selection: Option<Selection>,
    waiting: Option<Action>,
    /// Counts keys, wheel turns and mouse presses; newer input drops a waiting action.
    input: u64,
    /// The cell under the mouse, for the hover once a layer loads.
    pointer: Option<(u16, u16)>,
    /// Where the link under the mouse leads.
    pub hover: String,
    /// An OSC 52 sequence for the next frame.
    pub clipboard: Option<Vec<u8>>,
    session: Session,
    open: fn(&str) -> io::Result<()>,
}

impl Select {
    /// Selection on `book`, whose layers load on `jobs` and arrive as
    /// `Event::TextLayer` through `events`.
    pub(crate) fn new(book: Arc<PageCache>, jobs: Jobs, events: Sender<Event>) -> Self {
        Select {
            book,
            jobs,
            events,
            layers: HashMap::new(),
            loading: HashMap::new(),
            press: None,
            drag: None,
            selection: None,
            waiting: None,
            input: 0,
            pointer: None,
            hover: String::new(),
            clipboard: None,
            session: Session::current(),
            open: open_url,
        }
    }

    /// A key or a wheel turn, which may turn, rotate or zoom the page: an
    /// action still waiting for its layer drops, and the release of an
    /// active press or drag does nothing.
    pub(crate) fn interrupt(&mut self) {
        self.input += 1;
        self.waiting = None;
        self.end_press();
    }

    /// Ends an active press or drag, so its release does nothing: a resize
    /// moved its page, or a new frame shows another one.
    pub(crate) fn end_press(&mut self) {
        self.press = None;
        self.drag = None;
    }

    /// A new frame shows `pages`. The link under the mouse may have moved,
    /// and actions and layers of pages that left the screen drop.
    pub(crate) fn on_frame(&mut self, pages: &[usize]) {
        self.hover.clear();
        self.waiting = self.waiting.filter(|action| pages.contains(&action.hit.slot.page));
        self.loading.retain(|page, cancel| {
            let keep = pages.contains(page);
            if !keep {
                cancel.cancel();
            }
            keep
        });
    }

    /// The highlights of the selection on `page`.
    pub(crate) fn highlights(&self, page: usize) -> Arc<[PageRect]> {
        match (self.selection, self.layers.get(&page)) {
            (Some(selection), Some(Some(layer))) if selection.page == page => select(layer, selection.from, selection.to).1.into(),
            _ => Arc::from([]),
        }
    }

    pub(crate) fn mouse(&mut self, mouse: MouseEvent, view: &View) -> Outcome {
        let (x, y) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.input += 1;
                self.press(x, y, view)
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.input += 1;
                match view.hit(x, y) {
                    Some(hit) => self.act(Action { hit, selection: None, copy_link: true, input: self.input }, view),
                    None => Outcome::default(),
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(x, y, view),
            MouseEventKind::Up(MouseButton::Left) => self.release(x, y, view),
            MouseEventKind::Moved => {
                self.pointer = Some((x, y));
                self.hover(view);
                Outcome::default()
            }
            _ => Outcome::default(),
        }
    }

    /// Takes a layer that a job loaded. A selection on its page shows its
    /// highlights, and an action waiting for it runs.
    pub(crate) fn on_layer(&mut self, page: usize, layer: Result<Option<Arc<TextLayer>>>, view: &View) -> Outcome {
        if matches!(&layer, Err(error) if error.is_cancelled()) {
            self.loading.retain(|&loading, cancel| loading != page || !cancel.is_cancelled());
            return Outcome::default();
        }
        self.loading.remove(&page);
        self.layers.insert(page, layer.ok().flatten());
        if self.layers.len() > MAX_LAYERS {
            self.layers.retain(|&kept, _| kept == page || view.shows(kept));
        }
        self.hover(view);
        let mut outcome = Outcome { redraw: self.selection.is_some_and(|selection| selection.page == page), ..Outcome::default() };
        if let Some(action) = self.waiting.take_if(|action| action.hit.slot.page == page) {
            // Newer input, or a page that left the screen, drops the action.
            if action.input == self.input && view.shows(page) {
                outcome = self.finish(action, true);
            } else {
                outcome.status = Some(String::new());
            }
        }
        outcome
    }

    /// Names where the link under the mouse leads.
    fn hover(&mut self, view: &View) {
        let hit = self.pointer.and_then(|(x, y)| view.hit(x, y));
        self.hover = hit.and_then(|hit| self.link(hit)).map(label).unwrap_or_default();
    }

    /// Anchors a selection on the page under the mouse and clears the
    /// previous highlights.
    fn press(&mut self, x: u16, y: u16, view: &View) -> Outcome {
        self.press = Some((x, y));
        self.drag = None;
        let Some(hit) = view.hit(x, y) else { return Outcome::default() };
        self.load(hit.slot.page);
        self.drag = Some(hit);
        let had = self.selection.replace(Selection { page: hit.slot.page, from: (hit.x, hit.y), to: (hit.x, hit.y) }).is_some();
        Outcome { redraw: had, ..Outcome::default() }
    }

    fn drag_to(&mut self, x: u16, y: u16, view: &View) -> Outcome {
        let (Some(anchor), Some(selection)) = (self.drag, self.selection.as_mut()) else { return Outcome::default() };
        let hit = view.hit_in(anchor.slot, x, y);
        if selection.to == (hit.x, hit.y) {
            return Outcome::default();
        }
        selection.to = (hit.x, hit.y);
        Outcome { redraw: true, ..Outcome::default() }
    }

    /// Copies the selection, or follows the link under a click.
    fn release(&mut self, x: u16, y: u16, view: &View) -> Outcome {
        let Some(hit) = self.drag.take() else { return Outcome::default() };
        if self.press == Some((x, y)) {
            self.selection = None;
            return self.act(Action { hit, selection: None, copy_link: false, input: self.input }, view);
        }
        match self.selection {
            Some(selection) => self.act(Action { hit, selection: Some(selection), copy_link: false, input: self.input }, view),
            None => Outcome::default(),
        }
    }

    /// Runs `action` if its page still shows: now when its layer is loaded,
    /// and once it loads otherwise.
    fn act(&mut self, action: Action, view: &View) -> Outcome {
        let page = action.hit.slot.page;
        if !view.shows(page) {
            return Outcome::default();
        }
        if self.layers.contains_key(&page) {
            return self.finish(action, false);
        }
        self.waiting = Some(action);
        self.load(page);
        Outcome { status: Some("loading text…".to_owned()), ..Outcome::default() }
    }

    /// Copies the selected text, or follows or copies the link under the
    /// click. A selection that waited for its layer draws its highlights.
    fn finish(&mut self, action: Action, waited: bool) -> Outcome {
        let mut outcome = Outcome { status: Some(String::new()), ..Outcome::default() };
        if let Some(selection) = action.selection {
            let Some(Some(layer)) = self.layers.get(&selection.page) else { return outcome };
            let (text, _) = select(layer, selection.from, selection.to);
            if !text.is_empty() {
                outcome.status = Some(format!("copied {} characters", text.chars().count()));
                self.copy(text);
                outcome.redraw = waited;
            }
            return outcome;
        }
        match (self.link(action.hit).cloned(), action.copy_link) {
            (Some(Link::Page(page)), false) => outcome.go_to = Some(page),
            (Some(Link::Url(url)), false) => {
                let url = clean_url(&url);
                outcome.status = Some(match (self.open)(&url) {
                    Ok(()) => format!("→ {url}"),
                    Err(error) => format!("{url}  (open failed: {error})"),
                });
            }
            (Some(Link::Url(url)), true) => {
                let url = clean_url(&url);
                outcome.status = Some(format!("copied {url}"));
                self.copy(url);
            }
            _ => {}
        }
        outcome
    }

    /// The link under `hit`, when its page's layer is loaded. Starts loading
    /// the layer otherwise.
    fn link(&mut self, hit: Hit) -> Option<&Link> {
        self.load(hit.slot.page);
        link_at(self.layers.get(&hit.slot.page)?.as_ref()?, hit.x, hit.y)
    }

    /// Starts loading the layer of `page` unless it is loaded or loading.
    fn load(&mut self, page: usize) {
        if self.layers.contains_key(&page) || self.loading.contains_key(&page) {
            return;
        }
        let cancel = Cancel::new();
        self.loading.insert(page, cancel.clone());
        let (book, events) = (self.book.clone(), self.events.clone());
        self.jobs.spawn(Priority::Visible, &cancel.clone(), move || {
            let layer = book.text_layer(page, &cancel);
            let _ = events.send(Event::TextLayer { book, page, layer });
        });
    }

    /// Puts `text` on the clipboard: through the terminal with the next
    /// frame, and through local tools on a thread of its own.
    fn copy(&mut self, text: String) {
        self.clipboard = Some(osc52(&text));
        let (copy, session) = (COPIES.issue(), self.session.clone());
        thread::spawn(move || COPIES.copy(copy, &text, &session));
    }
}

/// The link at `x`, `y` of `page` for the native window, in fractions of
/// the page: PDF, DJVU and text pages have links in their text layers, and
/// web links lose their tracking parameters.
pub(crate) fn native_link(book: &dyn Book, page: usize, x: f64, y: f64) -> Option<Link> {
    if !matches!(book.kind(page), PageKind::Rendered | PageKind::Text) {
        return None;
    }
    let layer = book.text_layer(page, &Cancel::new()).ok()??;
    Some(match link_at(&layer, x, y)? {
        Link::Url(url) => Link::Url(clean_url(url)),
        link => link.clone(),
    })
}

/// What the status bar says about a link.
fn label(link: &Link) -> String {
    match link {
        Link::Page(page) => format!("p.{}", page + 1),
        Link::Url(url) => clean_url(url),
        _ => String::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
