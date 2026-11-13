//! The reader's state and what keys, mouse wheels, finished jobs and timers
//! do to it. Only `draw` turns the state into a frame.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, io, mem};

use crossbeam_channel::Sender;
use crossterm::event::{Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use yoyomi_core::{
    Book, Cancel, CellSize, Chapter, Grid, Jobs, Layer, PageCache, PageKind, Priority, Renderer, RendererKind, TextLayer, Transform, Upload,
    current_chapter,
};
use yoyomi_formats::{Opened, TextFormat};
use yoyomi_render::{Terminal, cell_size};
use yoyomi_search::{Progress, Search};
use yoyomi_state::{BookKey, Bookmarks, Position, Positions};
use yoyomi_view::{PageRequest, Pager, Resident, Shown, Strip, StripPosition, StripRequest, Update, ViewEvent};
use yoyomi_web::Server;

use crate::draw::{self, safe_text};
use crate::menus::{Action, Kind, Menu};
use crate::picker::{Picked, Picker};
use crate::search::{self, LineKey};
use crate::select::{self, Outcome, Select};
use crate::shot;

const ZOOM_STEP: f64 = 1.25;
const ZOOM_MAX: f64 = 8.0;
const FRAME_TIME: Duration = Duration::from_nanos(1_000_000_000 / 120);
/// Multiplexers can settle the window geometry after the first resize event.
const SETTLE_TIME: Duration = Duration::from_millis(300);

/// What reaches the event loop besides view events.
pub enum Event {
    Input(Input),
    /// Reading the terminal failed; no more input will come.
    InputFailed(io::Error),
    /// A book that opened on a job thread for the open `request`, or why it
    /// did not.
    Opened {
        request: u64,
        book: Result<Arc<dyn Book>, String>,
    },
    /// The outline of an open book, or why it could not be read.
    Chapters(Arc<PageCache>, Result<Vec<Chapter>, String>),
    /// Where a screenshot went, or why it failed.
    Shot(Result<PathBuf, String>),
    /// The text layer of `page` of `book`, loaded on a job thread.
    TextLayer {
        book: Arc<PageCache>,
        page: usize,
        layer: yoyomi_core::Result<Option<Arc<TextLayer>>>,
    },
    /// A step of a background search of `book`.
    Searched {
        book: Arc<PageCache>,
        progress: Progress,
    },
}

/// How the reader ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quit {
    /// `q`: forget the saved position.
    Clear,
    /// `Q`: save the position.
    Save,
}

/// The open book, its two views and its search, which stops when the book
/// closes.
struct Reader {
    book: Arc<PageCache>,
    /// The key the book's position and marks are saved under, made when it
    /// opened, so drawing and keys never ask the file system for it.
    key: BookKey,
    pager: Pager,
    strip: Strip,
    select: Select,
    search: Search,
}

impl Drop for Reader {
    /// Jobs still holding the book stop decoding it.
    fn drop(&mut self) {
        self.book.close();
    }
}

/// One book on screen: its views, its page and how it is turned and zoomed.
struct Pane {
    reader: Option<Reader>,
    /// The file name of the book.
    name: String,
    /// The open this pane waits for: its request and the page to show, or
    /// `None` for the saved position. The pane keeps its book meanwhile.
    opening: Option<(u64, Option<usize>)>,
    error: Option<String>,
    page: usize,
    quarter_turns: u8,
    invert: bool,
    zoom: f64,
    center: (f64, f64),
    /// Rows a plain-text page scrolled down.
    text_top: usize,
    /// The frame on screen, kept until a newer one arrives.
    shown: Option<Shown>,
    /// The zoom and center the shown frame was rendered with; keys change
    /// `zoom` and `center` before their frame arrives.
    shown_crop: (f64, (f64, f64)),
    /// Under tmux, the uploads of the images `shown` draws, which a frame
    /// sends whenever it draws them anew.
    uploads: Vec<Upload>,
    loading: bool,
    /// The book's outline once it loaded, or why it did not.
    chapters: Option<Result<Vec<Chapter>, String>>,
}

impl Default for Pane {
    fn default() -> Self {
        Pane {
            reader: None,
            name: String::new(),
            opening: None,
            error: None,
            page: 0,
            quarter_turns: 0,
            invert: false,
            zoom: 1.0,
            center: (0.5, 0.5),
            text_top: 0,
            shown: None,
            shown_crop: (1.0, (0.5, 0.5)),
            uploads: Vec::new(),
            loading: false,
            chapters: None,
        }
    }
}

impl Pane {
    fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.center = (0.5, 0.5);
        self.text_top = 0;
    }

    /// How the pane turns, zooms and inverts its pages, without selection
    /// highlights, as screenshots save them.
    fn transform(&self) -> Transform {
        Transform { quarter_turns: self.quarter_turns, zoom: self.zoom, center: self.center, invert: self.invert, ..Transform::default() }
    }

    /// Whether the pane reads the book with key `key`.
    fn reads(&self, key: &BookKey) -> bool {
        self.reader.as_ref().is_some_and(|reader| reader.key == *key)
    }
}

/// The rectangle a pane draws its image in, in screen cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageBox {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl ImageBox {
    /// Whether `layer` shows in the box on a screen of `screen_rows` rows,
    /// for painting and for the mouse alike. A sixel image shows only when
    /// all of it fits the box and it stays off the screen's last row, where
    /// it would scroll the screen; one drawn for a larger box waits for its
    /// render.
    pub fn shows(&self, layer: &Layer, screen_rows: u16) -> bool {
        let Layer::Sixel { grid, at, .. } = layer else { return true };
        at.col + grid.cols <= self.cols && at.row + grid.rows <= self.rows && self.y + at.row + grid.rows < screen_rows
    }
}

pub struct App {
    renderer: Arc<dyn Renderer>,
    probed: Terminal,
    jobs: Jobs,
    resident: Arc<Resident>,
    positions: Positions,
    pub bookmarks: Bookmarks,
    events: Sender<Event>,
    view_events: Sender<ViewEvent>,
    pub cols: u16,
    pub rows: u16,

    /// The left pane, and the right one in a split.
    panes: [Pane; 2],
    /// The pane keys act on.
    pub active: usize,
    pub split: bool,
    /// The last open request handed out.
    requests: u64,

    spread: bool,
    webtoon: bool,
    pub zen: bool,
    pub help: bool,
    /// The page of the help on screen, when it takes more than one.
    pub help_page: usize,
    /// The chapter or bookmark menu on screen.
    pub menu: Option<Menu>,
    pub picker: Picker,
    /// The pane the picker on screen opens a book in.
    pub pick_for: Option<usize>,
    count: String,
    status: String,
    /// The search line's input while `/` edits it.
    pub search_line: Option<String>,

    /// Kitty images to send before the next frame.
    pub uploads: Vec<Upload>,
    /// Sixel images in the shown scene still need painting.
    pub sixel_fresh: bool,
    /// Under tmux, the uploads of the images the last frame drew.
    pub drawn: Vec<Upload>,
    /// When the resize settles, and the cell size its first render used.
    settle: Option<(Instant, CellSize)>,
    last_tick: Instant,
    pub quit: Option<Quit>,
    /// The book and state `f` hands to the native window; the loop leaves
    /// the terminal for it.
    pub native: Option<(Arc<PageCache>, yoyomi_native::State)>,
    /// The position the native window saved with `Q`, for the active pane.
    native_save: Option<Position>,
    /// Why the terminal input stopped; the loop ends with it.
    pub input_error: Option<io::Error>,
    /// The browser reader, which starts on the first `e`.
    web: Server,
    /// Opens a link from `e` in the browser.
    open_browser: fn(&str) -> io::Result<()>,
}

impl App {
    /// A reader for a screen of `cols` x `rows` cells. Its bookmarks stay in
    /// memory until the caller sets the stored ones.
    pub fn new(
        renderer: Arc<dyn Renderer>,
        probed: Terminal,
        resident: Arc<Resident>,
        positions: Positions,
        events: Sender<Event>,
        view_events: Sender<ViewEvent>,
        (cols, rows): (u16, u16),
    ) -> Self {
        App {
            renderer,
            probed,
            jobs: Jobs::for_machine(),
            resident,
            positions,
            bookmarks: Bookmarks::load_from(None),
            events,
            view_events,
            cols,
            rows,
            panes: Default::default(),
            active: 0,
            split: false,
            requests: 0,
            spread: false,
            webtoon: false,
            zen: false,
            help: false,
            help_page: 0,
            menu: None,
            picker: Picker::new(std::env::current_dir().unwrap_or_default()),
            pick_for: None,
            count: String::new(),
            status: String::new(),
            search_line: None,
            uploads: Vec::new(),
            sixel_fresh: false,
            drawn: Vec::new(),
            settle: None,
            last_tick: Instant::now(),
            quit: None,
            native: None,
            native_save: None,
            input_error: None,
            web: Server::default(),
            open_browser: yoyomi_web::open_in_browser,
        }
    }

    /// Opens up to two books, side by side when there are two.
    pub fn open_books(&mut self, books: &[PathBuf]) {
        self.split = books.len() > 1;
        for (i, book) in books.iter().take(2).enumerate() {
            self.open_in(i, book, None);
        }
    }

    /// Opens `path` in pane `i` on a job thread, at `page` or else at the
    /// saved position; the title says so meanwhile.
    fn open_in(&mut self, i: usize, path: &Path, page: Option<usize>) {
        let request = self.expect_open(i, path, page);
        let (path, events) = (path.to_owned(), self.events.clone());
        self.jobs.spawn(Priority::Visible, &Cancel::new(), move || {
            let book = match open_book(&path) {
                Ok(Opened::Book(book)) => Ok(book),
                Ok(_) => Err("unsupported book".to_owned()),
                Err(error) => Err(error.to_string()),
            };
            let _ = events.send(Event::Opened { request, book });
        });
    }

    /// Makes pane `i` wait for the book at `path` and returns the request it
    /// will arrive with. A newer request replaces an older one.
    fn expect_open(&mut self, i: usize, path: &Path, page: Option<usize>) -> u64 {
        self.requests += 1;
        let pane = &mut self.panes[i];
        pane.name = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
        pane.opening = Some((self.requests, page));
        self.requests
    }

    /// Puts a book that opened into the pane waiting for it. A pane that
    /// closed or asked for another book since gets nothing, and the book
    /// closes.
    fn on_opened(&mut self, request: u64, book: Result<Arc<dyn Book>, String>) {
        let Some(i) = self.panes.iter().position(|pane| pane.opening.is_some_and(|(waiting, _)| waiting == request)) else {
            return;
        };
        let page = self.panes[i].opening.take().and_then(|(_, page)| page);
        match book {
            Ok(book) => self.opened(i, book, page),
            Err(error) => self.panes[i].error = Some(error),
        }
    }

    fn opened(&mut self, i: usize, book: Arc<dyn Book>, page: Option<usize>) {
        let book = Arc::new(PageCache::new(book));
        let pager = Pager::new(book.clone(), self.renderer.clone(), self.jobs.clone(), self.resident.clone(), self.view_events.clone());
        let strip = Strip::new(book.clone(), self.renderer.clone(), self.jobs.clone(), self.resident.clone(), self.view_events.clone());
        let select = Select::new(book.clone(), self.jobs.clone(), self.events.clone());
        let (searched, events) = (book.clone(), self.events.clone());
        let search = Search::new(book.inner().clone(), self.jobs.clone(), move |progress| {
            let _ = events.send(Event::Searched { book: searched.clone(), progress });
        });
        let last = book.len().saturating_sub(1);
        let key = BookKey::new(book.path());
        let saved = self.positions.get(&key);
        self.load_chapters(&book);
        if i == self.active && self.menu.as_ref().is_some_and(|menu| menu.kind == Kind::Chapters) {
            // The menu lists the chapters of the book this one replaces.
            self.close_overlay();
        }
        self.cancel_searches();
        let name = mem::take(&mut self.panes[i].name);
        self.panes[i] = Pane { reader: Some(Reader { book, key, pager, strip, select, search }), name, ..Pane::default() };
        self.serve_books();
        // A split keeps its layout; a single pane takes the saved one.
        if !self.split
            && let Some(saved) = &saved
        {
            self.webtoon = saved.webtoon;
            self.spread &= !saved.webtoon;
        }
        let (page, offset, scroll) = match (page, saved) {
            (Some(page), _) => (page, 0.0, 0.0),
            (None, Some(saved)) => (saved.page, saved.offset.clamp(0.0, 1.0), saved.scroll),
            (None, None) => (0, 0.0, 0.0),
        };
        self.panes[i].page = page.min(last);
        if self.webtoon {
            self.show_strip_again((StripPosition { page: page.min(last), offset }, scroll, 0.0));
        } else {
            self.show_pane(i);
        }
    }

    /// Reads the outline of `book` in the background, for the chapter menu
    /// and the chapter in the title.
    fn load_chapters(&self, book: &Arc<PageCache>) {
        let (book, events) = (book.clone(), self.events.clone());
        self.jobs.spawn(Priority::Neighbor, &Cancel::new(), move || {
            let chapters = book.chapters(&Cancel::new()).map_err(|error| error.to_string());
            let _ = events.send(Event::Chapters(book, chapters));
        });
    }

    /// Keeps the outline with the pane that still reads `book`, and fills
    /// the chapter menu waiting for it.
    fn on_chapters(&mut self, book: &Arc<PageCache>, chapters: Result<Vec<Chapter>, String>) {
        let Some(i) = self.pane_of(book) else { return };
        self.panes[i].chapters = Some(chapters);
        if i == self.active && self.menu.as_ref().is_some_and(|menu| menu.kind == Kind::Chapters) {
            self.open_chapters();
        }
    }

    /// The pane that still reads `book`. Every open makes its own cache, so
    /// at most one pane matches.
    fn pane_of(&self, book: &Arc<PageCache>) -> Option<usize> {
        self.panes.iter().position(|pane| pane.reader.as_ref().is_some_and(|reader| Arc::ptr_eq(&reader.book, book)))
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Input(Input::Key(key)) if key.kind != KeyEventKind::Release => self.key(key),
            Event::Input(Input::Mouse(mouse)) if self.reading() => match mouse.kind {
                MouseEventKind::ScrollDown => self.wheel(1, mouse.column),
                MouseEventKind::ScrollUp => self.wheel(-1, mouse.column),
                _ => self.mouse(mouse),
            },
            Event::Input(Input::Resize(cols, rows)) => self.resize(cols, rows, Instant::now()),
            Event::Input(_) => {}
            Event::InputFailed(error) => self.input_error = Some(error),
            Event::Opened { request, book } => self.on_opened(request, book),
            Event::Chapters(book, chapters) => self.on_chapters(&book, chapters),
            Event::Shot(Ok(path)) => self.status = format!("saved {}", path.display()),
            Event::Shot(Err(error)) => self.status = format!("screenshot failed: {error}"),
            Event::TextLayer { book, page, layer } => self.text_layer(&book, page, layer),
            Event::Searched { book, progress } => self.searched(&book, progress),
        }
    }

    /// Hands a finished job to the view that started it. Uploads go out
    /// whatever view sent them; frames show only from the view in use.
    pub fn on_view_event(&mut self, mut event: ViewEvent) {
        for i in 0..self.panes.len() {
            let Some(reader) = &mut self.panes[i].reader else { continue };
            let (update, active) = match reader.pager.on_event(event) {
                Ok(update) => (update, !self.webtoon),
                Err(other) => match reader.strip.on_event(other) {
                    Ok(update) => (update, self.webtoon),
                    Err(other) => {
                        event = other;
                        continue;
                    }
                },
            };
            match update.shown {
                Some(shown) if active => self.take_shown(i, shown, update.uploads),
                _ => self.uploads.extend(update.uploads),
            }
            return;
        }
    }

    /// When the loop must wake without input: the next frame of eased
    /// scrolling, or the end of a resize.
    pub fn deadline(&self) -> Option<Instant> {
        let reader = self.panes[self.active].reader.as_ref();
        let tick = reader.filter(|reader| self.webtoon && reader.strip.moving()).map(|_| self.last_tick + FRAME_TIME);
        match (tick, self.settle.map(|(at, _)| at)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn on_timer(&mut self, now: Instant) {
        if let Some(reader) = &mut self.panes[self.active].reader
            && self.webtoon
            && reader.strip.moving()
            && now >= self.last_tick + FRAME_TIME
        {
            self.last_tick = now;
            let before = reader.strip.position();
            let update = reader.strip.tick();
            // A tick that moved nothing, as at an end of the strip, leaves
            // the pane as it was.
            if update.shown.is_some() || reader.strip.position() != before {
                self.apply(update);
            }
        }
        if let Some((at, cell)) = self.settle
            && now >= at
        {
            self.settle = None;
            self.settled(cell);
        }
    }

    /// The second render after a resize. The same cells draw the same page,
    /// so it runs only when the cell size changed, a page failed, or under
    /// tmux, which drops an upload that arrives while it redraws.
    fn settled(&mut self, first: CellSize) {
        let cell = cell_size(&self.probed);
        if cell == first && !self.resident.resend() && !self.has_errors() {
            return;
        }
        self.renderer.set_cell_size(cell);
        self.render_again();
    }

    /// What the startup probe learned about the terminal.
    pub fn probed(&self) -> &Terminal {
        &self.probed
    }

    /// The cell size the renderer draws with.
    pub fn cell(&self) -> CellSize {
        self.renderer.cell_size()
    }

    fn resize(&mut self, cols: u16, rows: u16, now: Instant) {
        for reader in self.panes.iter_mut().filter_map(|pane| pane.reader.as_mut()) {
            reader.select.end_press();
        }
        self.cols = cols;
        self.rows = rows;
        // The selected row of a menu or the picker stays on the shorter screen.
        if let Some(menu) = &mut self.menu {
            menu.fit(rows);
        }
        self.picker.fit(rows);
        let cell = cell_size(&self.probed);
        self.renderer.set_cell_size(cell);
        self.settle = Some((now + SETTLE_TIME, cell));
        self.relayout();
    }

    /// Renders every pane again for new image boxes. A sixel image keeps the
    /// size it was drawn at, so it would cover a narrower box and its
    /// neighbors; it leaves the screen until the new render arrives.
    fn relayout(&mut self) {
        if self.renderer.kind() == RendererKind::Sixel {
            for pane in &mut self.panes {
                pane.shown = None;
            }
        }
        self.sixel_fresh = true;
        self.show_all();
    }

    fn has_errors(&self) -> bool {
        self.panes.iter().any(|pane| pane.error.is_some() || pane.shown.as_ref().is_some_and(|shown| !shown.errors.is_empty()))
    }

    // ---- requests to the views ---------------------------------------------

    /// How many panes show.
    pub fn pane_count(&self) -> usize {
        if self.split { 2 } else { 1 }
    }

    /// The first column and the width of pane `i`: the whole screen, or in a
    /// split two halves around a separator column.
    pub fn pane_columns(&self, i: usize) -> (u16, u16) {
        if !self.split {
            return (0, self.cols.max(1));
        }
        let left = self.cols.saturating_sub(1) / 2;
        if i == 0 { (0, left.max(1)) } else { (left + 1, self.cols.saturating_sub(left + 1).max(1)) }
    }

    /// The body of pane `i`: below the title and above the status row, or in
    /// focus mode the whole pane but the search line.
    fn body_box(&self, i: usize) -> ImageBox {
        let (x, cols) = self.pane_columns(i);
        let chrome = if self.zen { u16::from(self.search_line.is_some()) } else { 2 };
        ImageBox { x, y: u16::from(!self.zen), cols, rows: self.rows.saturating_sub(chrome).max(1) }
    }

    /// The columns and rows that come off the body, or a spread's half of it,
    /// for the box a page fits: a column on each side and a row, or in focus
    /// mode the bottom row under sixel, where an image would scroll the
    /// screen.
    fn margin(&self) -> Grid {
        if self.zen {
            return Grid::new(0, u16::from(self.renderer.kind() == RendererKind::Sixel && self.search_line.is_none()));
        }
        Grid::new(2, 1)
    }

    /// The image box of pane `i` without a spread: the body less the margin.
    /// The webtoon strip fills it.
    pub fn image_box(&self, i: usize) -> ImageBox {
        let (body, margin) = (self.body_box(i), self.margin());
        ImageBox {
            x: body.x + margin.cols / 2,
            y: body.y,
            cols: body.cols.saturating_sub(margin.cols).max(1),
            rows: body.rows.saturating_sub(margin.rows).max(1),
        }
    }

    /// The box pane `i`'s scene is placed in: the body for pages, which
    /// center in it, and the image box for the webtoon strip.
    pub fn scene_box(&self, i: usize) -> ImageBox {
        if self.webtoon { self.image_box(i) } else { self.body_box(i) }
    }

    fn grid(&self, i: usize) -> Grid {
        let image = self.image_box(i);
        Grid::new(image.cols, image.rows)
    }

    /// Asks the view in use for the active pane's current state.
    fn show(&mut self) {
        self.show_pane(self.active);
    }

    fn show_all(&mut self) {
        for i in 0..self.pane_count() {
            self.show_pane(i);
        }
    }

    /// Asks the view in use for pane `i`'s current state. The webtoon strip
    /// keeps its own position and the scrolling still to run.
    fn show_pane(&mut self, i: usize) {
        if self.webtoon {
            if let Some(place) = self.panes[i].reader.as_ref().map(|reader| kept_place(&reader.strip)) {
                self.show_strip_again(place);
            }
            return;
        }
        let request = self.page_request(i);
        let Some(reader) = &mut self.panes[i].reader else { return };
        let update = reader.pager.show(request);
        self.apply_to(i, update);
    }

    fn page_request(&self, i: usize) -> PageRequest {
        let pane = &self.panes[i];
        let spread = self.spread && !self.split;
        // The selection lies on one page, which may be either page of a spread.
        let shown = pane.page..pane.page + 1 + usize::from(spread);
        let (selected_page, highlights) = pane
            .reader
            .as_ref()
            .and_then(|reader| shown.map(|page| (page, reader.select.highlights(page))).find(|(_, highlights)| !highlights.is_empty()))
            .unwrap_or((pane.page, Arc::from([])));
        let transform = Transform { highlights, ..pane.transform() };
        let body = self.body_box(i);
        let grid = Grid::new(body.cols, body.rows);
        PageRequest { page: pane.page, spread, grid, margin: self.margin(), transform, selected_page, text_top: pane.text_top }
    }

    /// Shows the strip of the active pane, the only one in webtoon mode.
    fn show_strip(&mut self, at: StripPosition) {
        let i = self.active;
        let pane = &self.panes[i];
        let request = StripRequest { grid: self.grid(i), quarter_turns: pane.quarter_turns, invert: pane.invert };
        let Some(reader) = &mut self.panes[i].reader else { return };
        let update = reader.strip.show(at, request);
        self.apply(update);
    }

    /// Shows the strip of the active pane at `at` again, with `pending` rows
    /// of scrolling still to run and the `carry` kept for the next scroll, so
    /// rendering again does not stop a scroll under way.
    fn show_strip_again(&mut self, (at, pending, carry): (StripPosition, f64, f64)) {
        self.show_strip(at);
        if let Some(reader) = &mut self.panes[self.active].reader {
            reader.strip.scroll(pending);
            reader.strip.keep_carry(carry);
        }
    }

    /// Takes an update for the active pane.
    pub(crate) fn apply(&mut self, update: Update) {
        self.apply_to(self.active, update);
    }

    fn apply_to(&mut self, i: usize, update: Update) {
        match update.shown {
            Some(shown) => self.take_shown(i, shown, update.uploads),
            None => {
                self.uploads.extend(update.uploads);
                self.panes[i].loading = true;
            }
        }
    }

    /// Shows `shown` in pane `i`. Under tmux the pane keeps the uploads of
    /// the images on screen, which came with the frame; elsewhere they go out
    /// once.
    fn take_shown(&mut self, i: usize, shown: Shown, uploads: Vec<Upload>) {
        if self.resident.resend() {
            self.panes[i].uploads = uploads;
        } else {
            self.uploads.extend(uploads);
        }
        self.sixel_fresh = self.sixel_fresh || shown.scene.layers.iter().any(|layer| matches!(layer, Layer::Sixel { .. }));
        let spread = self.spread && !self.split;
        let pane = &mut self.panes[i];
        if let Some(reader) = &mut pane.reader {
            if pane.shown.as_ref().is_some_and(|old| old.page != shown.page) {
                reader.select.end_press();
            }
            reader.select.on_frame(&[shown.page, shown.page + usize::from(spread)]);
        }
        // The views send frames of their latest request only, which the current zoom and center made.
        pane.shown_crop = (pane.zoom, pane.center);
        pane.page = shown.page;
        pane.loading = false;
        pane.error = None;
        pane.shown = Some(shown);
    }

    /// Drops what the views keep and renders again. The strip renders again
    /// where it is, with the scrolling still to run.
    fn render_again(&mut self) {
        let place = self.panes[self.active].reader.as_ref().map(|reader| kept_place(&reader.strip));
        for reader in self.panes.iter_mut().filter_map(|pane| pane.reader.as_mut()) {
            reader.pager.clear();
            reader.strip.clear();
        }
        self.sixel_fresh = true;
        match place {
            Some(place) if self.webtoon => self.show_strip_again(place),
            _ => self.show_all(),
        }
    }

    // ---- keys ----------------------------------------------------------------

    /// Whether the mouse goes to the reader: no overlay and no search line.
    fn reading(&self) -> bool {
        !self.help && self.menu.is_none() && self.pick_for.is_none() && self.search_line.is_none()
    }

    fn key(&mut self, key: KeyEvent) {
        self.interrupt_selects();
        if self.help {
            // A key shows the help's next page; after the last it closes.
            self.help_page += 1;
            if self.help_page >= draw::help_pages(self.rows) {
                (self.help, self.help_page) = (false, 0);
                self.sixel_fresh = true;
            }
            return;
        }
        if self.search_line.is_some() {
            self.search_key(key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let name = match key.code {
            KeyCode::Char(c @ ('c' | 'n' | 'p')) if ctrl => format!("ctrl+{c}"),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => c.to_string(),
            KeyCode::Esc => "esc".to_owned(),
            KeyCode::Tab => "tab".to_owned(),
            KeyCode::Enter => "enter".to_owned(),
            KeyCode::Backspace => "backspace".to_owned(),
            KeyCode::Up => "up".to_owned(),
            KeyCode::Down => "down".to_owned(),
            KeyCode::Left => "left".to_owned(),
            KeyCode::Right => "right".to_owned(),
            KeyCode::PageUp => "pgup".to_owned(),
            KeyCode::PageDown => "pgdown".to_owned(),
            _ => return,
        };
        if self.pick_for.is_some() {
            self.pick_key(&name);
        } else if self.menu.is_some() {
            self.menu_key(&name);
        } else {
            self.press(&name);
        }
    }

    /// A key in reading mode, by its name as `key` writes it: a character,
    /// `ctrl+c`, `esc`, `pgdown` and the like.
    fn press(&mut self, key: &str) {
        self.status.clear();
        if let [digit @ b'0'..=b'9'] = key.as_bytes()
            && (*digit != b'0' || !self.count.is_empty())
        {
            self.count.push(char::from(*digit));
            return;
        }
        let active = self.active;
        match key {
            "q" | "ctrl+c" => self.quit = Some(Quit::Clear),
            "Q" => self.quit = Some(Quit::Save),
            "?" => self.help = true,
            "esc" => self.count.clear(),
            "j" => {
                let n = self.take_count(1);
                self.turn(active, n as isize, 1.0);
            }
            "k" => {
                let n = self.take_count(1);
                self.turn(active, -(n as isize), 1.0);
            }
            "J" => {
                let n = self.take_count(1);
                self.turn(active, n as isize, 0.5);
            }
            "K" => {
                let n = self.take_count(1);
                self.turn(active, -(n as isize), 0.5);
            }
            "g" => {
                let n = self.take_count(1);
                self.go_to(active, n - 1);
            }
            "G" => {
                let Some(len) = self.panes[active].reader.as_ref().map(|reader| reader.book.len()) else { return };
                if self.webtoon && self.count.is_empty() {
                    self.show_strip(StripPosition { page: len.saturating_sub(1), offset: 1.0 });
                    return;
                }
                let n = self.take_count(len);
                self.go_to(active, n.saturating_sub(1));
            }
            "s" => {
                if self.webtoon {
                    self.status = "spread is unavailable in webtoon mode (t to leave)".to_owned();
                    return;
                }
                if self.split {
                    self.status = "spread needs a single pane (v to unsplit)".to_owned();
                    return;
                }
                self.spread = !self.spread;
                self.show();
            }
            "t" => {
                if self.split {
                    self.status = "webtoon mode needs a single pane (v to unsplit)".to_owned();
                    return;
                }
                self.webtoon = !self.webtoon;
                self.spread = false;
                self.panes[active].reset_view();
                if self.webtoon {
                    self.show_strip(StripPosition { page: self.panes[active].page, offset: 0.0 });
                } else {
                    self.show();
                }
            }
            "z" => {
                self.zen = !self.zen;
                self.sixel_fresh = true;
                self.show_all();
            }
            "w" => {
                if self.split {
                    self.active = 1 - self.active;
                }
            }
            "v" | "O" if self.webtoon => self.status = "split is unavailable in webtoon mode (t to leave)".to_owned(),
            "v" => self.toggle_split(),
            "O" => {
                if !self.split {
                    self.cancel_searches();
                    self.split = true;
                    self.relayout();
                }
                self.pick(1);
            }
            "o" => self.pick(active),
            "x" => self.close_pane(active),
            "b" => self.toggle_bookmark(),
            "F" => self.menu = Some(Menu::bookmarks(&self.bookmarks.marks)),
            "tab" => self.open_chapters(),
            "i" => self.change(|pane| pane.invert = !pane.invert),
            "R" => self.change(|pane| pane.quarter_turns = (pane.quarter_turns + 1) & 3),
            "+" | "=" => self.zoom_by(ZOOM_STEP),
            "-" => self.zoom_by(1.0 / ZOOM_STEP),
            "0" => self.change(Pane::reset_view),
            "up" | "down" | "left" | "right" => self.pan(key),
            "r" => self.render_again(),
            "S" => self.screenshot(),
            "/" => self.open_search_line(),
            "n" => self.status = self.go_to_hit(true),
            "p" => self.status = self.go_to_hit(false),
            "e" => self.browse(),
            "f" => self.open_native(),
            _ => {}
        }
    }

    fn take_count(&mut self, default: usize) -> usize {
        let count = std::mem::take(&mut self.count);
        count.parse().ok().filter(|&n| n >= 1).unwrap_or(default)
    }

    /// Changes the view of the active pane's book and renders it.
    fn change(&mut self, change: impl FnOnce(&mut Pane)) {
        let pane = &mut self.panes[self.active];
        if pane.reader.is_some() {
            change(pane);
            self.show();
        }
    }

    /// Turns pane `i` by `delta` pages or spreads; in webtoon mode it scrolls
    /// `delta` times `fraction` of the viewport.
    fn turn(&mut self, i: usize, delta: isize, fraction: f64) {
        let rows = scroll_distance(self.image_box(i).rows, fraction);
        let spread = self.spread && !self.split;
        let pane = &mut self.panes[i];
        let Some(reader) = &mut pane.reader else { return };
        if self.webtoon {
            reader.strip.scroll(delta as f64 * rows);
            return;
        }
        let last = reader.book.len().saturating_sub(1) as isize;
        let page = pane.page as isize;
        let mut delta = delta;
        let step = if spread { 2 } else { 1 };
        // Past the end, a spread keeps its pairing: it stops on the last
        // spread, not on a lone last page.
        if spread && page + delta * step > last {
            delta = (last - page) / step;
        }
        self.go_to(i, (page + delta * step).max(0) as usize);
    }

    fn go_to(&mut self, i: usize, page: usize) {
        let pane = &mut self.panes[i];
        let Some(reader) = &pane.reader else { return };
        let page = page.min(reader.book.len().saturating_sub(1));
        if self.webtoon {
            self.show_strip(StripPosition { page, offset: 0.0 });
            return;
        }
        if page == pane.page && pane.shown.is_some() {
            return;
        }
        pane.page = page;
        pane.reset_view();
        self.show_pane(i);
    }

    /// The pane under the pointer at `column`: in a split, columns right of
    /// the middle belong to the right pane, and the separator to the left.
    fn pane_at(&self, column: u16) -> usize {
        usize::from(self.split && column > self.cols.saturating_sub(1) / 2)
    }

    /// The wheel turns the pane under the pointer at `column`.
    fn wheel(&mut self, delta: isize, column: u16) {
        self.status.clear();
        self.interrupt_selects();
        let i = self.pane_at(column);
        match &mut self.panes[i].reader {
            Some(reader) if self.webtoon => reader.strip.scroll(delta as f64),
            _ => self.turn(i, delta, 1.0),
        }
    }

    fn zoom_by(&mut self, factor: f64) {
        if self.webtoon {
            self.status = "webtoon mode fits pages to the viewport width".to_owned();
            return;
        }
        self.change(|pane| {
            pane.zoom = (pane.zoom * factor).clamp(1.0, ZOOM_MAX);
            if pane.zoom <= 1.001 {
                pane.reset_view();
            }
        });
    }

    fn pan(&mut self, key: &str) {
        // Plain-text pages scroll half a box; in a spread the longer page sets the range.
        let i = self.active;
        let total = self.panes[i].reader.as_ref().filter(|_| !self.webtoon).map_or(0, |reader| reader.pager.text_rows(&self.page_request(i)));
        if total > 0 {
            let rows = usize::from(self.image_box(i).rows);
            let step = (rows / 2).max(1);
            let pane = &mut self.panes[i];
            match key {
                "up" => pane.text_top = pane.text_top.saturating_sub(step),
                "down" => pane.text_top = (pane.text_top + step).min(total.saturating_sub(rows)),
                _ => return,
            }
            self.show();
            return;
        }
        if self.panes[i].zoom <= 1.001 {
            return;
        }
        self.change(|pane| {
            let step = 0.15 / pane.zoom;
            match key {
                "up" => pane.center.1 -= step,
                "down" => pane.center.1 += step,
                "left" => pane.center.0 -= step,
                _ => pane.center.0 += step,
            }
            let half = 0.5 / pane.zoom;
            pane.center = (pane.center.0.clamp(half, 1.0 - half), pane.center.1.clamp(half, 1.0 - half));
        });
    }

    fn screenshot(&mut self) {
        let pane = &self.panes[self.active];
        let Some(reader) = &pane.reader else { return };
        let (book, page, transform, events) = (reader.book.clone(), pane.page, pane.transform(), self.events.clone());
        let dir = std::env::var_os("YOYOMI_SHOT_DIR").filter(|dir| !dir.is_empty()).map_or_else(|| PathBuf::from("."), PathBuf::from);
        self.jobs.spawn(Priority::Visible, &Cancel::new(), move || {
            let saved = shot::save(book.as_ref(), page, &transform, &dir).map_err(|error| error.to_string());
            let _ = events.send(Event::Shot(saved));
        });
    }

    /// Puts `status` in the status bar and leaves focus mode, so it shows.
    fn report(&mut self, status: String) {
        self.status = status;
        if self.zen {
            self.zen = false;
            self.sixel_fresh = true;
            self.show_all();
        }
    }

    // ---- the mouse on pages with text ----------------------------------------

    /// Mouse presses, drags, releases and motion: selection and links. A
    /// left press makes the pane under it active. Every pane takes the mouse
    /// with its own pages, so the pane under the pointer acts, the other
    /// one's hover clears, and a drag stays on the page it began on.
    fn mouse(&mut self, mouse: MouseEvent) {
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.status.clear();
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.active = self.pane_at(mouse.column);
        }
        for i in 0..self.pane_count() {
            let view = self.select_view(i);
            let Some(reader) = &mut self.panes[i].reader else { continue };
            let outcome = reader.select.mouse(mouse, &view);
            self.select_outcome(i, outcome);
        }
    }

    /// Keys and wheel turns drop waiting actions and end presses in every pane.
    fn interrupt_selects(&mut self) {
        for reader in self.panes.iter_mut().filter_map(|pane| pane.reader.as_mut()) {
            reader.select.interrupt();
        }
    }

    fn text_layer(&mut self, book: &Arc<PageCache>, page: usize, layer: yoyomi_core::Result<Option<Arc<TextLayer>>>) {
        let Some(i) = self.pane_of(book) else { return };
        let view = self.select_view(i);
        let Some(reader) = &mut self.panes[i].reader else { return };
        let outcome = reader.select.on_layer(page, layer, &view);
        self.select_outcome(i, outcome);
    }

    /// The pages of pane `i` under the mouse. Webtoon mode and turned pages
    /// have none, and half blocks show text pages as text, which the terminal
    /// selects.
    fn select_view(&self, i: usize) -> select::View {
        let pane = &self.panes[i];
        let mut slots = Vec::new();
        if let (Some(reader), Some(shown)) = (&pane.reader, &pane.shown)
            && !self.webtoon
            && pane.quarter_turns == 0
        {
            let text_images = self.renderer.kind() != RendererKind::HalfBlock;
            slots = select::slots(shown, self.scene_box(i), self.rows, |page| match reader.book.kind(page) {
                PageKind::Rendered => true,
                PageKind::Text => text_images,
                _ => false,
            });
        }
        let (zoom, center) = pane.shown_crop;
        select::View { slots, zoom, center }
    }

    fn select_outcome(&mut self, i: usize, outcome: Outcome) {
        if let Some(status) = outcome.status {
            self.status = status;
        }
        if let Some(page) = outcome.go_to {
            self.go_to(i, page);
        } else if outcome.redraw {
            self.show_pane(i);
        }
    }

    /// The OSC 52 sequence of the latest copy, once.
    pub fn take_clipboard(&mut self) -> Option<Vec<u8>> {
        self.panes.iter_mut().find_map(|pane| pane.reader.as_mut()?.select.clipboard.take())
    }

    // ---- the browser -----------------------------------------------------------

    /// Serves the books of both panes to the browser and opens the active one
    /// at its page, with its inversion, through a new one-time link that the
    /// status bar shows.
    fn browse(&mut self) {
        let pane = &self.panes[self.active];
        if pane.reader.is_none() {
            self.status = "no book in this pane".to_owned();
            return;
        }
        let mut path = format!("b/{}/?p={}", self.active, pane.page);
        if pane.invert {
            path.push_str("&invert=1");
        }
        self.serve_books();
        self.status = match self.web.open(&path) {
            Ok(url) => match (self.open_browser)(&url) {
                Ok(()) => url,
                Err(error) => format!("{url}  (open failed: {error})"),
            },
            Err(error) => error.to_string(),
        };
    }

    /// Hands the browser the books of the panes, by pane, whenever they change.
    fn serve_books(&self) {
        self.web.set_books(self.panes.iter().map(|pane| pane.reader.as_ref().map(|reader| reader.book.clone() as Arc<dyn Book>)).collect());
    }

    // ---- the native window -----------------------------------------------------

    /// `f`: stops the searches and renders of both panes, gives their images
    /// back and asks the loop to show the active pane's book in the native
    /// window.
    fn open_native(&mut self) {
        if !yoyomi_native::available() {
            self.status = "native reader is unavailable in this build".to_owned();
            return;
        }
        let pane = &self.panes[self.active];
        let Some(reader) = &pane.reader else {
            self.status = "no book in this pane".to_owned();
            return;
        };
        let at = if self.webtoon { reader.strip.position() } else { StripPosition { page: pane.page, offset: 0.0 } };
        let (quarter_turns, zoom, center, invert) = (pane.quarter_turns, pane.zoom, pane.center, pane.invert);
        let spread = self.spread && !self.split;
        let state = yoyomi_native::State { page: at.page, offset: at.offset, webtoon: self.webtoon, quarter_turns, zoom, center, spread, invert };
        self.native = Some((reader.book.clone(), state));
        self.cancel_searches();
        for reader in self.panes.iter_mut().filter_map(|pane| pane.reader.as_mut()) {
            reader.select.end_press();
            reader.pager.clear();
            reader.strip.clear();
        }
    }

    /// Runs the native window `f` asked for, then takes back its state or
    /// quits as it says. Returns what to tell the shell.
    pub fn run_native(&mut self) -> Option<String> {
        let (book, state) = self.native.take()?;
        let outcome = yoyomi_native::run(book, self.jobs.clone(), state.clone(), select::native_link);
        self.native_done(state, outcome)
    }

    /// Acts on how the native window closed. A window that failed to open
    /// leaves the panes as `f` found them.
    fn native_done(&mut self, before: yoyomi_native::State, outcome: yoyomi_core::Result<yoyomi_native::Outcome>) -> Option<String> {
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                self.native_return(before);
                return Some(format!("native reader: {error}"));
            }
        };
        let state = outcome.state;
        match outcome.action {
            yoyomi_native::Action::Return => self.native_return(state),
            yoyomi_native::Action::Save => {
                let offset = if state.webtoon { state.offset } else { 0.0 };
                self.native_save = Some(Position { page: state.page, offset, scroll: 0.0, webtoon: state.webtoon });
                self.quit = Some(Quit::Save);
            }
            yoyomi_native::Action::Clear => self.quit = Some(Quit::Clear),
        }
        outcome.error.map(|error| format!("native reader: {error}"))
    }

    /// Gives the active pane the native window's state: webtoon mode only
    /// without a split, a spread only without a split or webtoon mode. Every
    /// pane renders and uploads again.
    fn native_return(&mut self, state: yoyomi_native::State) {
        self.webtoon = state.webtoon && !self.split;
        self.spread = state.spread && !self.split && !self.webtoon;
        let pane = &mut self.panes[self.active];
        if pane.page != state.page {
            pane.text_top = 0;
        }
        (pane.page, pane.quarter_turns, pane.invert, pane.zoom, pane.center) =
            (state.page, state.quarter_turns, state.invert, state.zoom, state.center);
        for pane in &mut self.panes {
            // Kitty ids in the old scenes may already name other images.
            pane.shown = None;
        }
        self.sixel_fresh = true;
        if self.webtoon {
            self.show_strip(StripPosition { page: state.page, offset: state.offset });
        } else {
            self.show_all();
        }
    }

    // ---- search ----------------------------------------------------------------

    fn open_search_line(&mut self) {
        let Some(reader) = &self.panes[self.active].reader else { return };
        if !yoyomi_search::tesseract_available() && !yoyomi_search::may_have_text(reader.book.as_ref()) {
            self.status = "OCR needs tesseract on PATH".to_owned();
            return;
        }
        self.search_line = Some(String::new());
        self.search_layout();
    }

    /// A key on the search line; enter replaces the reader's search with one
    /// of the active pane's book.
    fn search_key(&mut self, key: KeyEvent) {
        let Some(line) = &mut self.search_line else { return };
        match search::edit(line, key) {
            LineKey::Edit => return,
            LineKey::Close => {}
            LineKey::Run(term) => {
                self.cancel_searches();
                if let Some(reader) = &mut self.panes[self.active].reader {
                    reader.search.start(&term);
                    self.status = format!("OCR search {term:?}…");
                }
            }
        }
        self.search_line = None;
        self.search_layout();
    }

    /// In focus mode the search line takes a row from the image, so the image
    /// renders again when the line opens or closes.
    fn search_layout(&mut self) {
        if self.zen {
            self.sixel_fresh = true;
            self.show_all();
        }
    }

    /// The reader's one search and its pane. Each book has its own `Search`,
    /// but starting one cancels the others, so at most one has a term.
    fn search(&self) -> Option<(usize, &Search)> {
        self.panes.iter().enumerate().find_map(|(i, pane)| Some((i, &pane.reader.as_ref()?.search)).filter(|(_, search)| !search.term().is_empty()))
    }

    /// A step of the search of the pane reading `book`: a search that ends
    /// says so, and turns to its first hit while its pane is active.
    fn searched(&mut self, book: &Arc<PageCache>, progress: Progress) {
        let Some(i) = self.pane_of(book) else { return };
        let Some(reader) = &mut self.panes[i].reader else { return };
        if !reader.search.on_progress(progress) {
            return;
        }
        self.status = search::finished(&reader.search);
        if !reader.search.hits().is_empty() && i == self.active {
            let not_searched = search::not_searched(&reader.search);
            self.status = self.go_to_hit(true) + &not_searched;
        }
    }

    /// Turns the search's pane to its next or previous hit and makes it
    /// active, and returns what the status bar says.
    fn go_to_hit(&mut self, forward: bool) -> String {
        let Some((i, search)) = self.search() else { return search::NO_SEARCH.to_owned() };
        match search::hit(search, self.panes[i].page, forward) {
            Ok((page, status)) => {
                self.active = i;
                self.go_to(i, page);
                status
            }
            Err(status) => status,
        }
    }

    /// Stops the reader's search, as opening or closing a book and a change
    /// of the pane layout do.
    fn cancel_searches(&mut self) {
        for reader in self.panes.iter_mut().filter_map(|pane| pane.reader.as_mut()) {
            reader.search.cancel();
        }
    }

    // ---- panes, the picker and the menus ---------------------------------------

    /// `v`: unsplitting keeps the active pane; splitting moves to the right
    /// pane and picks a book for it when empty.
    fn toggle_split(&mut self) {
        self.cancel_searches();
        if self.split {
            if self.active == 1 {
                self.panes[0] = mem::take(&mut self.panes[1]);
            } else {
                self.panes[1] = Pane::default();
            }
            self.split = false;
            self.active = 0;
        } else {
            self.split = true;
            self.active = 1;
            if self.panes[1].reader.is_none() {
                self.pick(1);
            }
        }
        self.serve_books();
        self.relayout();
    }

    /// `x`: closes the book in pane `i`; a split becomes one pane.
    fn close_pane(&mut self, i: usize) {
        self.cancel_searches();
        self.panes[i] = Pane::default();
        if self.split {
            if i == 0 {
                self.panes.swap(0, 1);
            }
            self.split = false;
            self.active = 0;
        }
        self.serve_books();
        self.relayout();
    }

    /// Shows the picker for a book to open in pane `i`.
    fn pick(&mut self, i: usize) {
        self.pick_for = Some(i);
        self.picker.show();
    }

    fn pick_key(&mut self, key: &str) {
        let Some(i) = self.pick_for else { return };
        match self.picker.key(key, self.rows) {
            Picked::Stay => {}
            Picked::Cancel => self.close_overlay(),
            Picked::Book(path) => {
                self.close_overlay();
                self.open_in(i, &path, None);
            }
        }
    }

    fn close_overlay(&mut self) {
        self.menu = None;
        self.pick_for = None;
        self.sixel_fresh = true;
    }

    /// `b`: marks the active pane's page, or removes the mark there.
    fn toggle_bookmark(&mut self) {
        let pane = &self.panes[self.active];
        let Some(reader) = &pane.reader else { return };
        let (title, page) = (reader.book.title().to_owned(), pane.page);
        match self.bookmarks.toggle(&reader.key, &title, page) {
            Ok(true) => self.status = format!("bookmarked {title} p.{}", page + 1),
            Ok(false) => self.status = format!("removed bookmark {title} p.{}", page + 1),
            Err(error) => self.report(format!("bookmark failed: {error}")),
        }
    }

    /// `tab`: the active book's chapters, or a menu that waits for them. A
    /// book without any says so in the status bar instead.
    fn open_chapters(&mut self) {
        let pane = &self.panes[self.active];
        let Some(reader) = &pane.reader else { return };
        let menu = match &pane.chapters {
            None => Menu::loading_chapters(reader.book.title()),
            Some(Ok(chapters)) if !chapters.is_empty() => Menu::chapters(reader.book.title(), chapters, pane.page, self.rows),
            Some(result) => {
                let status = match result {
                    Err(error) => format!("chapters: {error}"),
                    Ok(_) => "no chapter info in this book".to_owned(),
                };
                self.close_overlay();
                self.report(status);
                return;
            }
        };
        match &mut self.menu {
            Some(open) if open.kind == Kind::Chapters => open.refresh(menu, self.rows),
            _ => self.menu = Some(menu),
        }
    }

    fn menu_key(&mut self, key: &str) {
        let Some(menu) = &mut self.menu else { return };
        let changed = match menu.key(key, self.rows) {
            Action::Stay => return,
            Action::Close => return self.close_overlay(),
            Action::Go(item) => {
                self.close_overlay();
                let i = self.active;
                return match item.book {
                    Some(book) if !self.panes[i].reads(&book) => self.open_in(i, book.path(), Some(item.page)),
                    _ => self.go_to(i, item.page),
                };
            }
            Action::Rename(item, name) => item.book.as_ref().map_or(Ok(()), |book| self.bookmarks.rename(book, item.page, &name)),
            Action::Delete(item) => item.book.as_ref().map_or(Ok(()), |book| self.bookmarks.remove(book, item.page)),
        };
        match changed {
            Ok(()) => {
                if let Some(menu) = &mut self.menu {
                    menu.refresh(Menu::bookmarks(&self.bookmarks.marks), self.rows);
                }
            }
            Err(error) => {
                self.close_overlay();
                self.report(format!("bookmark failed: {error}"));
            }
        }
    }

    // ---- what the frame shows -------------------------------------------------

    /// The frame pane `i` shows.
    pub fn shown(&self, i: usize) -> Option<&Shown> {
        self.panes[i].shown.as_ref()
    }

    /// Whether frames go through tmux to kitty, which needs them to start
    /// with a passthrough string.
    pub fn kitty_in_tmux(&self) -> bool {
        self.resident.resend() && self.renderer.kind() == RendererKind::Kitty
    }

    /// Under tmux, the uploads of the images pane `i` shows.
    pub fn shown_uploads(&self, i: usize) -> &[Upload] {
        &self.panes[i].uploads
    }

    /// Whether pane `i` has no book and waits for none.
    pub fn empty(&self, i: usize) -> bool {
        let pane = &self.panes[i];
        pane.reader.is_none() && pane.opening.is_none() && pane.error.is_none()
    }

    /// The title of pane `i`: a bookmark marker, the book title, the page
    /// numbers, the chapter, rotation and zoom marks, an ellipsis while a
    /// page loads, and the errors of a scene that still shows something.
    pub fn title(&self, i: usize) -> String {
        let pane = &self.panes[i];
        if pane.opening.is_some() {
            return format!("opening {} …", pane.name);
        }
        let Some(reader) = &pane.reader else {
            return if pane.name.is_empty() { "[empty]  press o to open".to_owned() } else { pane.name.clone() };
        };
        let len = reader.book.len();
        let mut title = if self.spread && !self.split && pane.page + 1 < len {
            format!("{}  {}-{}/{len}", reader.book.title(), pane.page + 1, pane.page + 2)
        } else {
            format!("{}  {}/{len}", reader.book.title(), pane.page + 1)
        };
        if self.bookmarks.has(&reader.key, pane.page) {
            title = format!("🔖 {title}");
        }
        let mut marks = Vec::new();
        if let Some(Ok(chapters)) = &pane.chapters
            && let Some(current) = current_chapter(chapters, pane.page)
        {
            let name = safe_text(&chapters[current].title);
            if !name.trim().is_empty() {
                marks.push(name.trim().to_owned());
            }
        }
        if pane.quarter_turns != 0 {
            marks.push(format!("{}°", u16::from(pane.quarter_turns) * 90));
        }
        if pane.zoom > 1.001 {
            // One decimal, left out when it is 0: two significant digits for zooms up to 8.
            let zoom = format!("{:.1}", pane.zoom);
            marks.push(format!("{}x", zoom.strip_suffix(".0").unwrap_or(&zoom)));
        }
        if !marks.is_empty() {
            title = format!("{title}  [{}]", marks.join(" "));
        }
        if pane.loading {
            title.push_str(" …");
        }
        // A spread writes its page errors into their halves.
        if let Some(shown) = &pane.shown
            && !shown.scene.layers.is_empty()
            && !shown.errors.is_empty()
            && !(self.spread && !self.split)
        {
            title = format!("{title}  {}", shown.errors.join("; "));
        }
        title
    }

    /// Errors to show in place of pane `i`'s image, when there is no image.
    pub fn body_errors(&self, i: usize) -> Vec<String> {
        let pane = &self.panes[i];
        if let Some(error) = &pane.error {
            return vec![error.clone()];
        }
        match &pane.shown {
            Some(shown) if shown.scene.layers.is_empty() => shown.errors.clone(),
            _ => Vec::new(),
        }
    }

    /// The errors of the pages of pane `i`'s spread that failed, each with
    /// the first column and the width of its half. The pager reports errors
    /// in slot order, so they go to the halves no layer fills.
    pub fn spread_errors(&self, i: usize) -> Vec<((u16, u16), String)> {
        let pane = &self.panes[i];
        let (Some(shown), Some(reader), Some(halves)) = (&pane.shown, &pane.reader, self.spread_halves(i)) else { return Vec::new() };
        if pane.error.is_some() {
            return Vec::new();
        }
        let half = halves[0].1;
        let slots = if shown.page + 1 < reader.book.len() { 2 } else { 1 };
        let filled = |right: bool| {
            shown.scene.layers.iter().any(|layer| match layer {
                Layer::KittyRows { at, .. }
                | Layer::Sixel { at, .. }
                | Layer::Cells { at, .. }
                | Layer::Text { at, .. }
                | Layer::Pixels { at, .. } => (at.col > half) == right,
                _ => false,
            })
        };
        let open = halves.into_iter().take(slots).enumerate().filter(|&(slot, _)| !filled(slot == 1)).map(|(_, columns)| columns);
        open.zip(shown.errors.iter().cloned()).collect()
    }

    /// The first column and the width of each half of pane `i`'s body in a
    /// spread: the left half takes half the columns but the gap, the right
    /// one the rest after it.
    pub fn spread_halves(&self, i: usize) -> Option<[(u16, u16); 2]> {
        if !self.spread || self.split || self.webtoon {
            return None;
        }
        let body = self.body_box(i);
        let half = body.cols.saturating_sub(1) / 2;
        Some([(body.x, half), (body.x + half + 1, body.cols.saturating_sub(half + 1))])
    }

    /// The left part of the status bar.
    pub fn status_left(&self) -> String {
        let mut left = format!(" {}", self.renderer.kind().name());
        if self.webtoon {
            left.push_str("  ·  webtoon");
        }
        let search = self.search().and_then(|(_, search)| search::progress(search)).unwrap_or_default();
        let port = self.web.port().map(|port| format!(":{port}")).unwrap_or_default();
        for part in [&self.count, &search, &port, &self.status] {
            if !part.is_empty() {
                left = format!("{left}  ·  {part}");
            }
        }
        if let Some(reader) = self.panes.iter().filter_map(|pane| pane.reader.as_ref()).find(|reader| !reader.select.hover.is_empty()) {
            left = format!("{left}  ·  → {}", reader.select.hover);
        }
        left
    }

    // ---- leaving -----------------------------------------------------------------

    /// Saves or forgets the positions as `quit` asks, closes the books and
    /// returns what went wrong with the saved positions.
    pub fn finish(&mut self) -> Option<String> {
        self.web.close();
        let native = self.native_save.take().zip(self.panes[self.active].reader.as_ref().map(|reader| reader.key.clone()));
        let readers: Vec<(Reader, usize)> = self.panes.iter_mut().filter_map(|pane| Some((pane.reader.take()?, pane.page))).collect();
        if readers.is_empty() {
            return None;
        }
        for (reader, page) in &readers {
            let key = &reader.key;
            match self.quit {
                Some(Quit::Save) if self.webtoon => {
                    let at = reader.strip.position();
                    self.positions.set(key, Position { page: at.page, offset: at.offset, scroll: reader.strip.pending(), webtoon: true });
                }
                Some(Quit::Save) => self.positions.set(key, Position { page: *page, ..Position::default() }),
                _ => self.positions.delete(key),
            }
        }
        if let (Some(Quit::Save), Some((position, key))) = (self.quit, native) {
            self.positions.set(&key, position);
        }
        let result = match self.quit {
            Some(Quit::Save) => self.positions.save().err().map(|error| format!("save progress: {error}")),
            _ => self.positions.save().err().map(|error| format!("clear progress: {error}")),
        };
        drop(readers);
        self.jobs.shutdown();
        result
    }
}

/// Opens the book at `path` for the reader and `-c`, with text books read by
/// the text crate. A file that looks like XML but fails as FB2 opens as a PDF
/// when `%PDF-` is within its first KiB.
pub fn open_book(path: &Path) -> yoyomi_core::Result<Opened> {
    match yoyomi_formats::open(path)? {
        Opened::Text(format) => match yoyomi_text::open(path, &format) {
            Err(_) if format == TextFormat::Fb2 && pdf_in_head(path) => yoyomi_formats::open_pdf(path),
            opened => opened,
        }
        .map(Opened::Book),
        opened => Ok(opened),
    }
}

/// Whether `%PDF-` is within the first KiB of the file at `path`.
fn pdf_in_head(path: &Path) -> bool {
    let mut head = Vec::with_capacity(1024);
    fs::File::open(path).and_then(|file| file.take(1024).read_to_end(&mut head)).is_ok() && head.windows(5).any(|window| window == b"%PDF-")
}

/// Rows one scroll moves: `fraction` of the viewport, at least one row.
/// What a strip keeps when it shows again: its position, the rows of
/// scrolling still to run and the fraction of a row kept for the next scroll.
fn kept_place(strip: &Strip) -> (StripPosition, f64, f64) {
    (strip.position(), strip.pending(), strip.carry())
}

fn scroll_distance(viewport_rows: u16, fraction: f64) -> f64 {
    (f64::from(viewport_rows) * fraction).max(1.0)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use crossbeam_channel::{Receiver, at, never, select, unbounded};
    use crossterm::event::{KeyEvent, MouseEvent};
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::Rect;
    use ratatui::{TerminalOptions, Viewport};
    use yoyomi_core::{At, Error, ImageIds, PageKind, Pixels, Result, RgbaImage, Scene};
    use yoyomi_render::{HalfBlock, Kitty, Sixel};

    use super::*;
    use crate::draw::tests::Recorder;
    use crate::draw::write_frame;

    /// A book of `len` small pages.
    pub(crate) struct FakeBook {
        pub len: usize,
        pub path: PathBuf,
    }

    impl Book for FakeBook {
        fn path(&self) -> &Path {
            &self.path
        }
        fn title(&self) -> &str {
            "Fake"
        }
        fn len(&self) -> usize {
            self.len
        }
        fn kind(&self, _page: usize) -> PageKind {
            PageKind::Image
        }
        fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
            cancel.check()?;
            if page >= self.len {
                return Err(Error::Format("no such page".into()));
            }
            Ok(Arc::new(RgbaImage::from_pixel(4, 2, [10, 20, 30, 255].into())))
        }
    }

    pub(crate) fn app_with(renderer: Arc<dyn Renderer>, cols: u16, rows: u16) -> (App, Receiver<Event>) {
        let (events, received) = unbounded();
        let (view_events, _) = unbounded();
        let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), false));
        let app = App::new(renderer, Terminal::default(), resident, Positions::load_from(None), events, view_events, (cols, rows));
        (app, received)
    }

    /// Hands `book` to pane `i` as the open of `path` there would.
    pub(crate) fn deliver(app: &mut App, i: usize, path: &str, book: std::result::Result<Arc<dyn Book>, String>) {
        let request = app.expect_open(i, Path::new(path), None);
        app.on_event(Event::Opened { request, book });
    }

    pub(crate) fn fake(len: usize, path: &str) -> Arc<dyn Book> {
        Arc::new(FakeBook { len, path: path.into() })
    }

    /// An 80 x 24 half-block reader with a book of `len` pages open.
    pub(crate) fn reader(len: usize) -> App {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        deliver(&mut app, 0, "/books/fake.cbz", Ok(fake(len, "/books/fake.cbz")));
        app
    }

    pub(crate) fn keys(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.on_event(Event::Input(Input::Key(KeyEvent::from(KeyCode::Char(c)))));
        }
    }

    pub(crate) fn shown(layers: Vec<Layer>, errors: Vec<String>) -> Update {
        Update { uploads: Vec::new(), shown: Some(Shown { page: 0, scene: Scene { layers }, errors }) }
    }

    #[test]
    fn counts_and_g_go_to_pages() {
        let mut app = reader(50);
        keys(&mut app, "j");
        assert_eq!(app.panes[0].page, 1);
        keys(&mut app, "2j");
        assert_eq!(app.panes[0].page, 3);
        keys(&mut app, "42G");
        assert_eq!(app.panes[0].page, 41);
        keys(&mut app, "G");
        assert_eq!(app.panes[0].page, 49);
        keys(&mut app, "g");
        assert_eq!(app.panes[0].page, 0);
        keys(&mut app, "5k");
        assert_eq!(app.panes[0].page, 0, "turns stop at the first page");
        keys(&mut app, "3g");
        assert_eq!(app.panes[0].page, 2);
    }

    #[test]
    fn esc_clears_a_count() {
        let mut app = reader(50);
        keys(&mut app, "12");
        assert!(app.status_left().ends_with("  ·  12"));
        app.on_event(Event::Input(Input::Key(KeyEvent::from(KeyCode::Esc))));
        keys(&mut app, "j");
        assert_eq!(app.panes[0].page, 1);
    }

    #[test]
    fn spread_stops_on_the_last_pair() {
        // Three pages pair as 1-2 and 3; from 2-3 the next spread is already on screen.
        let mut app = reader(3);
        keys(&mut app, "s");
        for (from, delta, want) in [(0, 1, 2), (1, 1, 1), (2, 1, 2), (0, 5, 2), (2, -1, 0)] {
            app.panes[0].page = from;
            app.turn(0, delta, 1.0);
            assert_eq!(app.panes[0].page, want, "turn {delta} from page {from}");
        }
    }

    #[test]
    fn zoom_and_pan_stay_in_bounds() {
        let mut app = reader(5);
        keys(&mut app, "-");
        assert_eq!(app.panes[0].zoom, 1.0);
        keys(&mut app, "++");
        assert_eq!(app.panes[0].zoom, 1.5625);
        for _ in 0..20 {
            app.press("left");
        }
        assert_eq!(app.panes[0].center.0, 0.5 / 1.5625);
        keys(&mut app, "0");
        assert_eq!((app.panes[0].zoom, app.panes[0].center), (1.0, (0.5, 0.5)));
        keys(&mut app, "t+");
        assert_eq!(app.panes[0].zoom, 1.0);
        assert!(app.status_left().contains("webtoon mode fits pages"));
    }

    #[test]
    fn arrows_scroll_plain_text_pages() {
        struct Text;
        impl Book for Text {
            fn path(&self) -> &Path {
                Path::new("/books/text.epub")
            }
            fn title(&self) -> &str {
                "Text"
            }
            fn len(&self) -> usize {
                2
            }
            fn kind(&self, _page: usize) -> PageKind {
                PageKind::Text
            }
            fn page(&self, _page: usize, _cancel: &Cancel) -> Result<Pixels> {
                Ok(Arc::new(RgbaImage::new(1, 1)))
            }
            fn page_text(&self, _page: usize) -> Option<Vec<String>> {
                Some(vec!["word ".repeat(800).trim_end().to_owned()])
            }
        }
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        deliver(&mut app, 0, "/books/text.epub", Ok(Arc::new(Text)));
        // 800 words wrap to 58 rows of 72 columns, and the box shows 21.
        for (key, top) in [("down", 10), ("down", 20), ("left", 20), ("down", 30), ("down", 37), ("down", 37), ("up", 27)] {
            app.press(key);
            assert_eq!(app.panes[0].text_top, top, "after {key}");
        }
        let Some([Layer::Text { lines, .. }]) = app.shown(0).map(|shown| &shown.scene.layers[..]) else { panic!("no plain text") };
        assert!(lines[0].starts_with("word word") && lines[20].starts_with("\u{2193} 11 more"), "{lines:?}");
        keys(&mut app, "j");
        assert_eq!((app.panes[0].page, app.panes[0].text_top), (1, 0), "a turn starts the page at its top");

        // In a split the arrows scroll the active pane's page only.
        app.split = true;
        deliver(&mut app, 1, "/books/text.epub", Ok(Arc::new(Text)));
        keys(&mut app, "w");
        app.press("down");
        assert_eq!((app.panes[0].text_top, app.panes[1].text_top), (0, 10));
    }

    #[test]
    fn focus_mode_takes_the_chrome_rows() {
        let mut app = reader(5);
        assert_eq!(app.image_box(0), ImageBox { x: 1, y: 1, cols: 78, rows: 21 });
        keys(&mut app, "z");
        assert_eq!(app.image_box(0), ImageBox { x: 0, y: 0, cols: 80, rows: 24 });
    }

    #[test]
    fn focus_mode_keeps_the_bottom_row_free_for_sixel() {
        let (mut app, _) = app_with(Arc::new(Sixel::new(CellSize::FALLBACK, 256)), 80, 24);
        keys(&mut app, "z");
        assert_eq!(app.image_box(0), ImageBox { x: 0, y: 0, cols: 80, rows: 23 });
    }

    #[test]
    fn pages_center_in_the_body_and_fit_inside_its_margin() {
        let place = |app: &App| {
            let request = app.page_request(0);
            (app.scene_box(0), request.grid, request.margin)
        };
        let mut app = reader(5);
        assert_eq!(place(&app), (ImageBox { x: 0, y: 1, cols: 80, rows: 22 }, Grid::new(80, 22), Grid::new(2, 1)));
        keys(&mut app, "z");
        assert_eq!(place(&app), (ImageBox { x: 0, y: 0, cols: 80, rows: 24 }, Grid::new(80, 24), Grid::new(0, 0)));
        // Under sixel the bottom row stays free, unless the search line takes it.
        let (mut app, _) = app_with(Arc::new(Sixel::new(CellSize::FALLBACK, 256)), 81, 25);
        keys(&mut app, "z");
        assert_eq!(place(&app), (ImageBox { x: 0, y: 0, cols: 81, rows: 25 }, Grid::new(81, 25), Grid::new(0, 1)));
        app.search_line = Some(String::new());
        assert_eq!(place(&app), (ImageBox { x: 0, y: 0, cols: 81, rows: 24 }, Grid::new(81, 24), Grid::new(0, 0)));
        // In a split, the right pane's body starts after the separator.
        let mut app = reader(5);
        app.split = true;
        assert_eq!(app.scene_box(1), ImageBox { x: 40, y: 1, cols: 40, rows: 22 });
    }

    #[test]
    fn webtoon_scroll_distances() {
        assert_eq!(scroll_distance(41, 1.0), 41.0);
        assert_eq!(scroll_distance(41, 0.5), 20.5);
        assert_eq!(scroll_distance(1, 0.5), 1.0);
    }

    #[test]
    fn webtoon_keys_scroll_instead_of_turning() {
        let mut app = reader(10);
        keys(&mut app, "t");
        keys(&mut app, "jJs");
        assert_eq!(app.panes[0].page, 0);
        assert!(app.status_left().contains("webtoon  ·  spread is unavailable"));
    }

    #[test]
    fn f_hands_the_active_pane_to_the_native_window() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        keys(&mut app, "f");
        let unavailable = "native reader is unavailable in this build";
        let status = if yoyomi_native::available() { "no book in this pane" } else { unavailable };
        assert!(app.status_left().ends_with(status));
        let mut app = reader(5);
        keys(&mut app, "2jR+f");
        if !yoyomi_native::available() {
            assert!(app.status_left().ends_with(unavailable) && app.native.is_none());
            return;
        }
        let (book, state) = app.native.clone().expect("f asks for the window");
        assert_eq!(book.path(), Path::new("/books/fake.cbz"));
        assert_eq!((state.page, state.quarter_turns, state.zoom, state.webtoon), (2, 1, ZOOM_STEP, false));
    }

    #[test]
    fn the_native_window_returns_its_state_to_the_active_pane() {
        let mut app = reader(10);
        app.panes[0].text_top = 5;
        let state = yoyomi_native::State { page: 7, quarter_turns: 2, invert: true, spread: true, ..yoyomi_native::State::default() };
        let outcome = yoyomi_native::Outcome { state, action: yoyomi_native::Action::Return, error: None };
        assert_eq!(app.native_done(yoyomi_native::State::default(), Ok(outcome)), None);
        let pane = &app.panes[0];
        assert_eq!((pane.page, pane.quarter_turns, pane.invert, app.spread, app.quit), (7, 2, true, true, None));
        assert_eq!(pane.text_top, 0, "another page shows from its top");

        // A failed window leaves the pane as f found it, and says why.
        let before = yoyomi_native::State { page: 3, ..yoyomi_native::State::default() };
        let message = app.native_done(before, Err(yoyomi_core::Error::unsupported("no window")));
        assert_eq!(message.as_deref(), Some("native reader: no window"));
        assert_eq!((app.panes[0].page, app.spread), (3, false));

        let state = yoyomi_native::State { page: 4, offset: 0.25, webtoon: true, ..yoyomi_native::State::default() };
        let outcome = yoyomi_native::Outcome { state, action: yoyomi_native::Action::Save, error: Some("page 5: broken".into()) };
        assert_eq!(app.native_done(yoyomi_native::State::default(), Ok(outcome)).as_deref(), Some("native reader: page 5: broken"));
        assert_eq!(app.quit, Some(Quit::Save));
        assert_eq!(app.native_save, Some(Position { page: 4, offset: 0.25, scroll: 0.0, webtoon: true }));
    }

    #[test]
    fn screenshots_save_the_page_without_selection_highlights() {
        let app = crate::select::tests::selecting_reader();
        assert!(!app.page_request(0).transform.highlights.is_empty(), "the page on screen shows the selection");
        assert!(app.panes[0].transform().highlights.is_empty(), "S saves the page with this transform");
    }

    #[test]
    fn selection_highlights_show_in_spreads_and_on_turned_pages() {
        let mut app = crate::select::tests::selecting_reader();
        for (key, spread, quarter_turns) in [("s", true, 0), ("R", true, 1), ("s", false, 1)] {
            keys(&mut app, key);
            let request = app.page_request(0);
            assert_eq!((request.spread, request.transform.quarter_turns), (spread, quarter_turns));
            assert!(!request.transform.highlights.is_empty() && request.selected_page == 0, "after {key}");
        }
    }

    #[test]
    fn e_opens_the_book_in_the_browser() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        keys(&mut app, "e");
        assert!(app.status_left().ends_with("no book in this pane"));
        let mut app = reader(5);
        app.open_browser = |_| Ok(());
        keys(&mut app, "e");
        let port = app.web.port().expect("the server runs");
        let first = app.status_left();
        assert!(first.contains(&format!("  ·  :{port}  ·  http://127.0.0.1:{port}/open/")), "{first}");
        keys(&mut app, "e");
        assert_ne!(app.status_left(), first, "e makes a new link");
        app.open_browser = |_| Err(io::Error::other("no browser"));
        keys(&mut app, "e");
        assert!(app.status_left().ends_with("(open failed: no browser)"));
        app.finish();
        assert_eq!(app.web.port(), None, "the server stops with the reader");
    }

    #[test]
    fn e_serves_both_panes_and_opens_the_active_one() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.split = true;
        deliver(&mut app, 0, "/books/a.cbz", Ok(fake(10, "/books/a.cbz")));
        deliver(&mut app, 1, "/books/b.cbz", Ok(fake(10, "/books/b.cbz")));
        thread_local! {
            static URL: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
        }
        app.open_browser = |url| {
            URL.with(|opened| *opened.borrow_mut() = url.to_owned());
            Ok(())
        };
        keys(&mut app, "w3ji");
        keys(&mut app, "e");
        let port = app.web.port().expect("the server runs");
        let get = |path: &str| {
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
            std::io::Write::write_all(&mut stream, request.as_bytes()).unwrap();
            let mut reply = String::new();
            std::io::Read::read_to_string(&mut stream, &mut reply).unwrap();
            reply
        };
        // The one-time link leads to the active pane's book at its page.
        let url = URL.with(|opened| opened.take());
        let redirect = get(url.strip_prefix(&format!("http://127.0.0.1:{port}")).expect("a local link"));
        let location = redirect.lines().find_map(|line| line.strip_prefix("Location: ")).expect("a redirect");
        let root = location.strip_suffix("b/1/?p=3&invert=1").expect(location);
        for (book, served) in [("b/0/", true), ("b/1/", true), ("b/2/", false)] {
            assert_eq!(get(&format!("{root}{book}")).starts_with("HTTP/1.1 200"), served, "{book}");
        }
        app.finish();
    }

    #[test]
    fn quit_keys_choose_the_save_policy() {
        let mut app = reader(5);
        keys(&mut app, "q");
        assert_eq!(app.quit, Some(Quit::Clear));
        let mut app = reader(5);
        keys(&mut app, "Q");
        assert_eq!(app.quit, Some(Quit::Save));
        let mut app = reader(5);
        app.on_event(Event::Input(Input::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))));
        assert_eq!(app.quit, Some(Quit::Clear));
    }

    #[test]
    fn help_closes_on_any_key() {
        let mut app = reader(5);
        app.rows = 40;
        keys(&mut app, "?");
        assert!(app.help);
        keys(&mut app, "j");
        assert!(!app.help);
        assert_eq!(app.panes[0].page, 0, "the key that closes help does nothing else");
    }

    #[test]
    fn help_longer_than_the_screen_turns_its_pages_on_any_key() {
        let mut app = reader(5);
        keys(&mut app, "?");
        keys(&mut app, "j");
        assert!(app.help && app.help_page == 1, "a 24-row screen shows the second page");
        keys(&mut app, "j");
        assert!(!app.help && app.help_page == 0, "the last page closes");
        assert_eq!(app.panes[0].page, 0, "the keys that page and close help do nothing else");
    }

    #[test]
    fn the_wheel_turns_pages() {
        let mut app = reader(5);
        let wheel = |kind| Event::Input(Input::Mouse(MouseEvent { kind, column: 0, row: 0, modifiers: KeyModifiers::NONE }));
        app.on_event(wheel(MouseEventKind::ScrollDown));
        app.on_event(wheel(MouseEventKind::ScrollDown));
        app.on_event(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.panes[0].page, 1);
    }

    #[test]
    fn titles_show_pages_and_marks() {
        let mut app = reader(40);
        app.panes[0].page = 2;
        app.panes[0].loading = false;
        assert_eq!(app.title(0), "Fake  3/40");
        keys(&mut app, "s");
        app.panes[0].loading = false;
        assert_eq!(app.title(0), "Fake  3-4/40");
        app.panes[0].page = 39;
        assert_eq!(app.title(0), "Fake  40/40", "a spread on the last page shows it alone");
        keys(&mut app, "sR+");
        app.panes[0].loading = false;
        assert_eq!(app.title(0), "Fake  40/40  [90° 1.2x]");
        keys(&mut app, "+R");
        assert_eq!(app.title(0), "Fake  40/40  [180° 1.6x] …");
    }

    #[test]
    fn titles_show_opening_and_errors() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.open_in(0, Path::new("/nonexistent/missing.cbz"), None);
        assert_eq!(app.title(0), "opening missing.cbz …");
        app.on_event(Event::Opened { request: app.requests, book: Err("open missing.cbz: not found".into()) });
        assert_eq!(app.title(0), "missing.cbz");
        assert_eq!(app.body_errors(0), ["open missing.cbz: not found"]);

        let mut app = reader(4);
        app.apply(shown(Vec::new(), vec!["page 1: corrupt".into()]));
        assert_eq!(app.body_errors(0), ["page 1: corrupt"]);
        let text = Layer::Text { lines: vec!["left".to_owned()].into(), at: At::default() };
        app.apply(shown(vec![text], vec!["page 2: corrupt".into()]));
        assert!(app.body_errors(0).is_empty());
        assert_eq!(app.title(0), "Fake  1/4  page 2: corrupt");
    }

    #[test]
    fn the_scene_stays_until_a_new_one_arrives() {
        let mut app = reader(4);
        let text = Layer::Text { lines: vec!["page one".to_owned()].into(), at: At::default() };
        app.apply(shown(vec![text], Vec::new()));
        keys(&mut app, "j");
        assert!(app.shown(0).is_some(), "a turn keeps the old frame while the new one loads");
        assert!(app.title(0).ends_with(" …"));
    }

    #[test]
    fn open_books_in_the_background() {
        let (mut app, received) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.open_in(0, Path::new("/nonexistent/missing.cbz"), None);
        let event = received.recv_timeout(Duration::from_secs(5)).unwrap();
        app.on_event(event);
        assert!(app.panes[0].opening.is_none() && app.panes[0].reader.is_none());
        assert!(app.body_errors(0)[0].contains("missing.cbz"));
    }

    #[test]
    fn the_resize_settles_only_when_it_must() {
        let mut app = reader(4);
        let now = Instant::now();
        app.resize(100, 30, now);
        assert_eq!(app.image_box(0).cols, 98);
        assert!(app.deadline().is_some());
        app.on_timer(now + SETTLE_TIME);
        assert!(app.settle.is_none() && app.deadline().is_none());
    }

    /// An 80 x 24 kitty reader of the fixture `name`, and the receiver of its
    /// view jobs. With `tmux`, uploads go out with the frames that draw them
    /// and a resize renders again.
    fn kitty_reader(name: &str, tmux: bool) -> (App, Receiver<ViewEvent>) {
        let (events, received) = unbounded();
        let (view_events, view_received) = unbounded();
        let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), tmux));
        let renderer = Arc::new(Kitty::new(CellSize::FALLBACK, false));
        let mut app = App::new(renderer, Terminal::default(), resident, Positions::load_from(None), events, view_events, (80, 24));
        app.open_books(&[fixture(name)]);
        app.on_event(received.recv_timeout(Duration::from_secs(20)).expect("the book opens"));
        (app, view_received)
    }

    /// A kitty reader of `testdata/webtoon.cbz` in webtoon mode.
    fn webtoon_reader(tmux: bool) -> (App, Receiver<ViewEvent>) {
        let (mut app, view_received) = kitty_reader("webtoon.cbz", tmux);
        keys(&mut app, "t");
        run_views(&mut app, &view_received, |app| app.shown(0).is_some() && app.deadline().is_none());
        (app, view_received)
    }

    #[test]
    fn half_block_webtoon_refuses_text_pages_in_the_body() {
        struct Words;
        impl Book for Words {
            fn path(&self) -> &Path {
                Path::new("/books/words.epub")
            }
            fn title(&self) -> &str {
                "Words"
            }
            fn len(&self) -> usize {
                3
            }
            fn kind(&self, _page: usize) -> PageKind {
                PageKind::Text
            }
            fn page(&self, _page: usize, _cancel: &Cancel) -> Result<Pixels> {
                Ok(Arc::new(RgbaImage::new(40, 60)))
            }
            fn page_text(&self, _page: usize) -> Option<Vec<String>> {
                Some(vec!["words".to_owned()])
            }
        }
        let (events, _) = unbounded();
        let (view_events, views) = unbounded();
        let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), false));
        let renderer = Arc::new(HalfBlock::new(CellSize::FALLBACK));
        let mut app = App::new(renderer, Terminal::default(), resident, Positions::load_from(None), events, view_events, (80, 24));
        deliver(&mut app, 0, "/books/words.epub", Ok(Arc::new(Words)));
        let refusal = "text pages in webtoon need pixel rendering";
        for key in ["t", "z"] {
            keys(&mut app, key);
            run_views(&mut app, &views, |app| !app.panes[0].loading && app.deadline().is_none());
            let rows: Vec<String> = (0..24).map(|y| screen_row(&app, y)).collect();
            // The body's middle row, with and without the title and status rows.
            assert!(rows[11].trim().starts_with(refusal), "after {key}: {rows:?}");
            assert!(!rows[0].contains(refusal), "the title stays as it is");
        }
    }

    #[test]
    fn under_tmux_a_page_from_the_cache_goes_out_again() {
        for tmux in [true, false] {
            let (mut app, views) = kitty_reader("nested.cbz", tmux);
            let options = TerminalOptions { viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)) };
            let written = Recorder::default();
            let mut terminal = ratatui::Terminal::with_options(CrosstermBackend::new(written.clone()), options).unwrap();
            let mut frame = |app: &mut App| {
                let before = written.text().len();
                write_frame(&mut terminal, app).unwrap();
                written.text()[before..].to_owned()
            };
            let on_page = |page: usize| move |app: &App| app.shown(0).is_some_and(|shown| shown.page == page);
            run_views(&mut app, &views, on_page(0));
            let Some(Layer::KittyRows { id, .. }) = app.shown(0).and_then(|shown| shown.scene.layers.first().cloned()) else {
                panic!("a kitty page")
            };
            let page0 = format!("a=T,q=2,f=100,t=d,i={},", id.0);
            assert_eq!(frame(&mut app).matches(&page0).count(), 1, "the first show");

            keys(&mut app, "j");
            run_views(&mut app, &views, on_page(1));
            assert_eq!(frame(&mut app).matches(&page0).count(), 0);
            keys(&mut app, "k");
            assert!(on_page(0)(&app), "page 0 comes from the cache");
            assert_eq!(frame(&mut app).matches(&page0).count(), usize::from(tmux), "tmux {tmux}: the return");
            assert_eq!(frame(&mut app).matches(&page0).count(), 0);
            keys(&mut app, "?");
            assert_eq!(frame(&mut app).matches(&page0).count(), 0);
            // The help takes two pages of a 24-row screen.
            keys(&mut app, "jj");
            assert!(!app.help && on_page(0)(&app));
            assert_eq!(frame(&mut app).matches(&page0).count(), usize::from(tmux), "tmux {tmux}: help closed");
        }
    }

    /// Runs the event loop on finished view jobs and timers until `done`.
    pub(crate) fn run_views(app: &mut App, view_received: &Receiver<ViewEvent>, done: impl Fn(&App) -> bool) {
        let limit = Instant::now() + Duration::from_secs(20);
        while !done(app) {
            let timer = app.deadline().map_or_else(never, at);
            select! {
                recv(view_received) -> event => app.on_view_event(event.unwrap()),
                recv(timer) -> _ => app.on_timer(Instant::now()),
                recv(at(limit)) -> _ => panic!("the views did not settle"),
            }
        }
    }

    /// Where the strip of the active pane is, and the rows it still scrolls.
    fn strip_place(app: &App) -> (StripPosition, f64) {
        let strip = &app.panes[app.active].reader.as_ref().expect("an open book").strip;
        (strip.position(), strip.pending())
    }

    fn assert_near(got: StripPosition, want: StripPosition, after: &str) {
        assert!(got.page == want.page && (got.offset - want.offset).abs() < 1e-9, "after {after}: {got:?}, want {want:?}");
    }

    #[test]
    fn r_and_resizes_keep_the_webtoon_position() {
        let (mut app, views) = webtoon_reader(true);
        keys(&mut app, "jJ");
        run_views(&mut app, &views, |app| app.deadline().is_none());
        let (before, _) = strip_place(&app);
        assert_ne!(before.page, 0);
        keys(&mut app, "r");
        assert_near(strip_place(&app).0, before, "r");
        run_views(&mut app, &views, |app| app.deadline().is_none());
        assert_near(strip_place(&app).0, before, "r rendered");

        // Under tmux the settled resize renders again.
        let now = Instant::now();
        app.resize(100, 30, now);
        assert_near(strip_place(&app).0, before, "a resize");
        app.on_timer(now + SETTLE_TIME);
        assert_near(strip_place(&app).0, before, "a settled resize");
        run_views(&mut app, &views, |app| app.deadline().is_none());
        assert_near(strip_place(&app).0, before, "a settled resize rendered");
    }

    #[test]
    fn a_half_row_kept_for_the_next_scroll_survives_r() {
        // 21 rows: half a viewport is 10.5 rows, which kitty cannot show.
        let settle = |app: &mut App, views: &Receiver<ViewEvent>, keys_: &str| {
            keys(app, keys_);
            run_views(app, views, |app| app.deadline().is_none());
        };
        let (mut whole, views) = webtoon_reader(false);
        settle(&mut whole, &views, "j");
        let (mut halves, views) = webtoon_reader(false);
        settle(&mut halves, &views, "J");
        let (before, _) = strip_place(&halves);
        settle(&mut halves, &views, "r");
        assert_near(strip_place(&halves).0, before, "r");
        settle(&mut halves, &views, "J");
        assert_near(strip_place(&halves).0, strip_place(&whole).0, "J r J against j");
    }

    #[test]
    fn the_scroll_still_to_run_is_saved_and_restored() {
        let (mut app, _views) = webtoon_reader(false);
        keys(&mut app, "j");
        let (at, pending) = strip_place(&app);
        app.quit = Some(Quit::Save);
        app.finish();
        let path = fixture("webtoon.cbz");
        assert_eq!(app.positions.get(&BookKey::new(&path)), Some(Position { page: at.page, offset: at.offset, scroll: pending, webtoon: true }));

        let (mut app, _views) = webtoon_reader(false);
        app.positions.set(&BookKey::new(&path), Position { page: 2, offset: 0.5, scroll: 7.0, webtoon: true });
        let Ok(Opened::Book(book)) = yoyomi_formats::open(&path) else { panic!("the fixture opens as a comic") };
        deliver(&mut app, 0, path.to_str().expect("a UTF-8 path"), Ok(book));
        let (at, pending) = strip_place(&app);
        assert_eq!((at.page, at.offset, pending), (2, 0.5, 7.0));
    }

    #[test]
    fn a_scroll_past_the_end_of_the_strip_leaves_no_loading_mark() {
        let (mut app, views) = webtoon_reader(false);
        keys(&mut app, "G");
        run_views(&mut app, &views, |app| !app.panes[0].loading && app.deadline().is_none());
        assert!(!app.title(0).ends_with(" …"), "{}", app.title(0));
        keys(&mut app, "j");
        run_views(&mut app, &views, |app| app.deadline().is_none());
        assert!(!app.title(0).ends_with(" …"), "{}", app.title(0));
    }

    #[test]
    fn showing_the_strip_again_keeps_the_scroll_still_to_run() {
        let (mut app, views) = webtoon_reader(false);
        for key in ["z", "i", "R", "0", "r", "resize"] {
            keys(&mut app, "j");
            let (_, pending) = strip_place(&app);
            assert!(pending > 0.0);
            match key {
                "resize" => app.resize(90, 30, Instant::now()),
                key => keys(&mut app, key),
            }
            assert_eq!(strip_place(&app).1, pending, "{key} keeps the scroll");
            run_views(&mut app, &views, |app| app.deadline().is_none());
        }
        // `g` and `G` jump, and a jump starts without it.
        for key in ["g", "G"] {
            keys(&mut app, "k");
            keys(&mut app, key);
            assert_eq!(strip_place(&app).1, 0.0, "{key} drops the scroll");
            run_views(&mut app, &views, |app| app.deadline().is_none());
        }
    }

    // ---- panes ---------------------------------------------------------------

    fn press_code(app: &mut App, code: KeyCode) {
        app.on_event(Event::Input(Input::Key(KeyEvent::from(code))));
    }

    fn book_of(app: &App, i: usize) -> Arc<PageCache> {
        app.panes[i].reader.as_ref().expect("the pane has a book").book.clone()
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(name)
    }

    #[test]
    fn books_open_in_the_background() {
        let mut app = reader(3);
        let first = book_of(&app, 0);
        // The pane keeps its book while the next one opens, and says so.
        let older = app.expect_open(0, Path::new("/books/next.cbz"), Some(1));
        assert_eq!(app.title(0), "opening next.cbz …");
        // A second request replaces the first, whose book arrives too late.
        let newer = app.expect_open(0, Path::new("/books/next.cbz"), Some(2));
        app.on_event(Event::Opened { request: older, book: Ok(fake(3, "/books/next.cbz")) });
        assert!(Arc::ptr_eq(&book_of(&app, 0), &first) && app.panes[0].opening.is_some());
        // The awaited book replaces the old one at the page asked for, and the old one closes.
        app.on_event(Event::Opened { request: newer, book: Ok(fake(3, "/books/next.cbz")) });
        let second = book_of(&app, 0);
        assert_eq!(second.path(), Path::new("/books/next.cbz"));
        assert_eq!(app.panes[0].page, 2);
        assert!(!app.title(0).contains("opening"));
        assert!(first.page(0, &Cancel::new()).is_err(), "the replaced book closed");
        // A failed open reports its error and keeps the book.
        deliver(&mut app, 0, "/books/absent.cbz", Err("open absent.cbz: not found".into()));
        assert!(Arc::ptr_eq(&book_of(&app, 0), &second) && app.panes[0].opening.is_none());
        assert_eq!(app.body_errors(0), ["open absent.cbz: not found"]);
        // A pane that closes while its book opens gets none.
        let gone = app.expect_open(0, Path::new("/books/gone.cbz"), None);
        app.close_pane(0);
        app.on_event(Event::Opened { request: gone, book: Ok(fake(3, "/books/gone.cbz")) });
        assert!(app.panes[0].reader.is_none() && app.empty(0));
        assert_eq!(app.title(0), "[empty]  press o to open");
    }

    #[test]
    fn two_books_from_the_command_line_open_side_by_side() {
        let (mut app, received) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.open_books(&[fixture("nested.cbz"), fixture("solid.cbr")]);
        assert!(app.split && app.active == 0);
        while app.panes.iter().any(|pane| pane.opening.is_some()) {
            app.on_event(received.recv_timeout(Duration::from_secs(5)).expect("both books open"));
        }
        assert!(app.title(0).starts_with("nested  1/5"), "{}", app.title(0));
        assert!(app.title(1).starts_with("solid  1/3"), "{}", app.title(1));
    }

    #[test]
    fn split_panes_turn_on_their_own() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.split = true;
        deliver(&mut app, 0, "/books/a.cbz", Ok(fake(10, "/books/a.cbz")));
        deliver(&mut app, 1, "/books/b.cbz", Ok(fake(10, "/books/b.cbz")));
        assert_eq!((app.pane_columns(0), app.pane_columns(1)), ((0, 39), (40, 40)));
        assert_eq!(app.image_box(1), ImageBox { x: 41, y: 1, cols: 38, rows: 21 });
        keys(&mut app, "j");
        keys(&mut app, "w2j");
        assert_eq!((app.active, app.panes[0].page, app.panes[1].page), (1, 1, 2));
        let wheel = Event::Input(Input::Mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 10, row: 5, modifiers: KeyModifiers::NONE }));
        app.on_event(wheel);
        assert_eq!((app.panes[0].page, app.panes[1].page), (2, 2), "the wheel turns the pane under the pointer");
        keys(&mut app, "s");
        assert!(app.status_left().ends_with("spread needs a single pane (v to unsplit)"));
        keys(&mut app, "t");
        assert!(app.status_left().ends_with("webtoon mode needs a single pane (v to unsplit)"));
        // Unsplitting keeps the active pane.
        keys(&mut app, "v");
        assert!(!app.split && app.active == 0);
        assert_eq!(book_of(&app, 0).path(), Path::new("/books/b.cbz"));
        assert!(app.panes[1].reader.is_none());
    }

    #[test]
    fn a_click_makes_the_pane_under_it_active() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.split = true;
        deliver(&mut app, 0, "/books/a.cbz", Ok(fake(10, "/books/a.cbz")));
        deliver(&mut app, 1, "/books/b.cbz", Ok(fake(10, "/books/b.cbz")));
        let click = |kind, column| Event::Input(Input::Mouse(MouseEvent { kind, column, row: 0, modifiers: KeyModifiers::NONE }));
        // Column 39 is the separator, which belongs to the left pane.
        for (kind, column, active) in [
            (MouseEventKind::Down(MouseButton::Left), 40, 1),
            (MouseEventKind::Down(MouseButton::Right), 10, 1),
            (MouseEventKind::Moved, 10, 1),
            (MouseEventKind::Down(MouseButton::Left), 39, 0),
            (MouseEventKind::Down(MouseButton::Left), 79, 1),
        ] {
            app.on_event(click(kind, column));
            assert_eq!(app.active, active, "{kind:?} at column {column}");
        }
        keys(&mut app, "j");
        assert_eq!((app.panes[0].page, app.panes[1].page), (0, 1), "keys act on the clicked pane");
    }

    #[test]
    fn the_mouse_does_nothing_while_the_search_line_is_open() {
        let mut app = reader(5);
        app.search_line = Some(String::new());
        for kind in [MouseEventKind::ScrollDown, MouseEventKind::Down(MouseButton::Left)] {
            app.on_event(Event::Input(Input::Mouse(MouseEvent { kind, column: 0, row: 0, modifiers: KeyModifiers::NONE })));
        }
        assert_eq!(app.panes[0].page, 0);
        app.search_line = None;
        app.on_event(Event::Input(Input::Mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 0, row: 0, modifiers: KeyModifiers::NONE })));
        assert_eq!(app.panes[0].page, 1);
    }

    #[test]
    fn x_closes_a_pane_and_ends_the_split() {
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.split = true;
        deliver(&mut app, 0, "/books/a.cbz", Ok(fake(10, "/books/a.cbz")));
        deliver(&mut app, 1, "/books/b.cbz", Ok(fake(10, "/books/b.cbz")));
        keys(&mut app, "x");
        assert!(!app.split && app.active == 0);
        assert_eq!(book_of(&app, 0).path(), Path::new("/books/b.cbz"));
        keys(&mut app, "x");
        assert!(app.empty(0));
    }

    #[test]
    fn split_keys_open_the_picker_for_the_right_pane() {
        let mut app = reader(5);
        keys(&mut app, "O");
        assert!(app.split && app.active == 0 && app.pick_for == Some(1));
        press_code(&mut app, KeyCode::Esc);
        assert!(app.split && app.pick_for.is_none(), "esc leaves the split for the next pick");
        keys(&mut app, "vv");
        assert!(app.split && app.active == 1 && app.pick_for == Some(1), "v moves to the empty right pane and picks");

        let mut app = reader(5);
        keys(&mut app, "t");
        for key in ["v", "O"] {
            keys(&mut app, key);
            assert!(app.status_left().ends_with("split is unavailable in webtoon mode (t to leave)"), "{key}");
        }
        assert!(!app.split && app.pick_for.is_none());
    }

    #[test]
    fn the_picker_opens_a_book_in_the_active_pane() {
        let dir = tempfile::tempdir().unwrap();
        fs::copy(fixture("nested.cbz"), dir.path().join("Comic.CBZ")).unwrap();
        let (mut app, received) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.picker = Picker::new(dir.path().to_owned());
        keys(&mut app, "o");
        assert_eq!(app.pick_for, Some(0));
        let (buf, _) = crate::draw::tests::render(&app);
        assert_eq!((0..80).map(|x| buf[(x, 2)].symbol()).collect::<String>().trim_end(), "> Comic.CBZ");
        press_code(&mut app, KeyCode::Enter);
        assert!(app.pick_for.is_none());
        assert_eq!(app.title(0), "opening Comic.CBZ …");
        app.on_event(received.recv_timeout(Duration::from_secs(5)).expect("the book opens"));
        assert_eq!(book_of(&app, 0).len(), 5);
    }

    #[test]
    fn a_pdf_behind_xml_looking_junk_opens() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/CropBox[0 0 100 100]>>endobj\n4 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 200]>>endobj\ntrailer<</Root 1 0 R>>\n";
        let write = |name: &str, bytes: String| {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            path
        };
        // A shifted PDF signature does not override a valid FB2.
        let fb2 = write("note.fb2", "<FictionBook><body><section><p>%PDF-1.4</p></section></body></FictionBook>".to_owned());
        assert!(matches!(open_book(&fb2), Ok(Opened::Book(book)) if book.kind(0) == PageKind::Text));
        if !crate::select::tests::have(&["pdfinfo", "pdftoppm", "pdftohtml"]) {
            return;
        }
        for path in [write("shifted.pdf", "junk\n".repeat(100) + pdf), write("xml-shifted.pdf", format!("<junk>\n{pdf}"))] {
            match open_book(&path) {
                Ok(Opened::Book(book)) => assert_eq!(book.len(), 2, "{}", path.display()),
                other => panic!("{}: {other:?}", path.display()),
            }
        }
        // A relative name with a leading dash reaches the PDF tools as a path, not an option.
        let mut dashed = tempfile::Builder::new().prefix("-v").rand_bytes(0).tempfile_in(".").unwrap();
        io::Write::write_all(&mut dashed, format!("<junk>\n{pdf}").as_bytes()).unwrap();
        let name = Path::new(dashed.path().file_name().expect("a file name"));
        match open_book(name) {
            Ok(Opened::Book(book)) => assert_eq!(book.len(), 2, "{}", name.display()),
            other => panic!("{}: {other:?}", name.display()),
        }
    }

    #[test]
    fn the_picker_opens_a_text_book_in_the_chosen_pane() {
        let dir = tempfile::tempdir().unwrap();
        let fb2 = "<FictionBook><body><section><title><p>First</p></title><p>Text</p></section></body></FictionBook>";
        fs::write(dir.path().join("Story.FB2"), fb2).unwrap();
        let (mut app, received) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        deliver(&mut app, 0, "/books/a.cbz", Ok(fake(3, "/books/a.cbz")));
        app.picker = Picker::new(dir.path().to_owned());
        keys(&mut app, "O");
        assert_eq!(screen_row(&app, 2), "> Story.FB2");
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.title(1), "opening Story.FB2 …");
        while app.panes[1].opening.is_some() {
            app.on_event(received.recv_timeout(Duration::from_secs(5)).expect("the book opens"));
        }
        assert!(app.body_errors(1).is_empty(), "{:?}", app.body_errors(1));
        assert_eq!(book_of(&app, 1).path().file_name(), Some("Story.FB2".as_ref()));
        assert_eq!(book_of(&app, 0).path(), Path::new("/books/a.cbz"), "the left pane keeps its book");
    }

    #[test]
    fn a_resize_keeps_the_selected_row_on_screen() {
        let outline: Vec<Chapter> = (0..30).map(|i| chapter(&format!("Part {i}"), 0, 0)).collect();
        let (mut app, loaded) = outlined(outline);
        app.on_event(loaded);
        press_code(&mut app, KeyCode::Tab);
        keys(&mut app, "G");
        app.on_event(Event::Input(Input::Resize(80, 10)));
        assert!(screen_row(&app, 7).starts_with("> Part 29"), "{}", screen_row(&app, 7));
        press_code(&mut app, KeyCode::Esc);

        let dir = tempfile::tempdir().unwrap();
        for i in 0..30 {
            fs::write(dir.path().join(format!("{i:02}.cbz")), b"").unwrap();
        }
        app.on_event(Event::Input(Input::Resize(80, 24)));
        app.picker = Picker::new(dir.path().to_owned());
        keys(&mut app, "oG");
        app.on_event(Event::Input(Input::Resize(80, 10)));
        assert_eq!(screen_row(&app, 7), "> 29.cbz");
    }

    #[test]
    fn the_picker_moves_with_the_filepicker_keys() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..30 {
            fs::write(dir.path().join(format!("{i:02}.cbz")), b"").unwrap();
        }
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.picker = Picker::new(dir.path().to_owned());
        keys(&mut app, "o");
        // The list shows 20 rows from screen row 2; PageDown moves as J, ctrl+p as k.
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        for (key, row, entry) in
            [(KeyEvent::from(KeyCode::PageDown), 21, "20"), (ctrl('p'), 20, "19"), (KeyEvent::from(KeyCode::PageUp), 2, "00"), (ctrl('n'), 3, "01")]
        {
            app.on_event(Event::Input(Input::Key(key)));
            assert_eq!(screen_row(&app, row), format!("> {entry}.cbz"), "{key:?}");
        }
    }

    #[test]
    fn splitting_drops_a_sixel_page_wider_than_its_pane() {
        let (mut app, _) = app_with(Arc::new(Sixel::new(CellSize::FALLBACK, 256)), 80, 24);
        deliver(&mut app, 0, "/books/a.cbz", Ok(fake(3, "/books/a.cbz")));
        let wide = Layer::Sixel { data: b"<wide>".as_slice().into(), grid: Grid::new(78, 21), at: At::default() };
        app.apply(shown(vec![wide], Vec::new()));
        assert_eq!(crate::draw::tests::render(&app).1, 1);
        keys(&mut app, "v");
        press_code(&mut app, KeyCode::Esc);
        let fits = |app: &App, i: usize| {
            let cols = app.image_box(i).cols;
            app.shown(i).is_none_or(|shown| {
                shown.scene.layers.iter().all(|layer| !matches!(layer, Layer::Sixel { grid, at, .. } if at.col + grid.cols > cols))
            })
        };
        assert!(app.split && fits(&app, 0) && fits(&app, 1));
        let (buf, paints) = crate::draw::tests::render(&app);
        assert_eq!(paints, 0, "nothing paints over the separator and the right pane");
        assert_eq!(buf[(39, 5)].symbol(), "│");
    }

    // ---- bookmarks -----------------------------------------------------------

    #[test]
    fn bookmarks_mark_pages_and_open_books_at_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = reader(10);
        app.bookmarks = Bookmarks::load_from(Some(dir.path().join("bookmarks.json")));
        keys(&mut app, "3gb");
        assert!(app.status_left().ends_with("bookmarked Fake p.3"));
        assert!(app.title(0).starts_with("🔖 Fake  3/10"), "{}", app.title(0));
        keys(&mut app, "j");
        assert!(app.title(0).starts_with("Fake  4/10"));
        keys(&mut app, "kb");
        assert!(app.status_left().ends_with("removed bookmark Fake p.3"));
        keys(&mut app, "b");
        let other = BookKey::new(Path::new("/books/other.cbz"));
        app.bookmarks.toggle(&other, "Other", 6).unwrap();

        keys(&mut app, "F");
        let (buf, _) = crate::draw::tests::render(&app);
        let row = |y: u16| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert_eq!(row(0).trim_end(), " Bookmarks");
        assert!(row(1).starts_with("> Fake ") && row(2).starts_with("  Other "), "{:?}", [row(1), row(2)]);
        keys(&mut app, "rCover");
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.bookmarks.marks[0].name, "Cover");
        keys(&mut app, "jd");
        assert_eq!(app.bookmarks.marks.len(), 1);
        assert!(app.menu.is_some(), "the menu stays open");

        app.bookmarks.toggle(&other, "Other", 6).unwrap();
        press_code(&mut app, KeyCode::Esc);
        keys(&mut app, "FG");
        press_code(&mut app, KeyCode::Enter);
        assert!(app.menu.is_none());
        assert!(matches!(app.panes[0].opening, Some((_, Some(6)))), "another book opens at the mark");
        app.panes[0].opening = None;
        keys(&mut app, "gFl");
        assert_eq!(app.panes[0].page, 2, "a mark in the open book turns to its page");
    }

    #[cfg(unix)]
    #[test]
    fn marks_and_positions_stay_when_the_opened_path_stops_resolving() {
        let dir = tempfile::tempdir().unwrap();
        let (real, link) = (dir.path().join("real"), dir.path().join("link"));
        fs::create_dir(&real).unwrap();
        fs::write(real.join("book.cbz"), b"").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (mut app, _) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        app.bookmarks = Bookmarks::load_from(Some(dir.path().join("bookmarks.json")));
        let opened = link.join("book.cbz");
        let opened = opened.to_str().expect("a UTF-8 path");
        deliver(&mut app, 0, opened, Ok(fake(3, opened)));
        keys(&mut app, "b");
        let key = BookKey::new(&real.join("book.cbz"));
        assert_eq!(app.bookmarks.marks[0].book, key);

        fs::remove_file(&link).unwrap();
        assert!(app.title(0).starts_with("🔖 Fake  1/3"), "{}", app.title(0));
        app.quit = Some(Quit::Save);
        app.finish();
        assert_eq!(app.positions.get(&key), Some(Position::default()));
    }

    #[test]
    fn failed_bookmarks_leave_focus_mode_to_say_so() {
        let mut app = reader(3);
        keys(&mut app, "zb");
        assert!(!app.zen);
        assert!(app.status_left().contains("bookmark failed: "), "{}", app.status_left());
    }

    // ---- chapters ------------------------------------------------------------

    /// A fake book with an outline.
    struct OutlineBook(FakeBook, Vec<Chapter>);

    impl Book for OutlineBook {
        fn path(&self) -> &Path {
            self.0.path()
        }
        fn title(&self) -> &str {
            "Outline"
        }
        fn len(&self) -> usize {
            self.0.len
        }
        fn kind(&self, page: usize) -> PageKind {
            self.0.kind(page)
        }
        fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
            self.0.page(page, cancel)
        }
        fn chapters(&self, cancel: &Cancel) -> Result<Vec<Chapter>> {
            cancel.check()?;
            Ok(self.1.clone())
        }
    }

    fn chapter(title: &str, page: usize, depth: usize) -> Chapter {
        Chapter { title: title.into(), page, depth }
    }

    /// A reader on a book with `outline`, and the outline as it loaded.
    fn outlined(outline: Vec<Chapter>) -> (App, Event) {
        let (mut app, received) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        let book = OutlineBook(FakeBook { len: 3, path: "/books/outline.cbz".into() }, outline);
        deliver(&mut app, 0, "/books/outline.cbz", Ok(Arc::new(book)));
        let loaded = received.recv_timeout(Duration::from_secs(5)).expect("the outline loads");
        (app, loaded)
    }

    fn screen_row(app: &App, y: u16) -> String {
        let (buf, _) = crate::draw::tests::render(app);
        (0..app.cols).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_owned()
    }

    #[test]
    fn the_chapter_menu_loads_without_blocking() {
        let (mut app, loaded) = outlined(vec![chapter("First", 0, 0), chapter("Last", 2, 0)]);
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(screen_row(&app, 1), "  loading chapters…");
        keys(&mut app, "/Last");
        app.on_event(loaded);
        assert_eq!(screen_row(&app, 0), " Chapters — Outline  (1/2)", "the filter typed while loading stays");
        assert_eq!(screen_row(&app, 3), " /Last▏  fuzzy filter · enter keep · esc clear");
        press_code(&mut app, KeyCode::Enter);
        press_code(&mut app, KeyCode::Enter);
        assert!(app.menu.is_none());
        assert_eq!(app.panes[0].page, 2);
        assert!(app.title(0).contains("[Last]"), "{}", app.title(0));
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(screen_row(&app, 2), format!("> Last{} p.3", " ".repeat(36)), "a loaded outline opens at once, on the current chapter");
    }

    #[test]
    fn late_chapters_reach_only_their_book() {
        // A closed menu stays closed, and the outline stays for the next tab.
        let (mut app, loaded) = outlined(vec![chapter("Loaded", 1, 0)]);
        press_code(&mut app, KeyCode::Tab);
        press_code(&mut app, KeyCode::Esc);
        app.on_event(loaded);
        assert!(app.menu.is_none() && matches!(&app.panes[0].chapters, Some(Ok(chapters)) if chapters.len() == 1));

        // A replaced book, or a pane that closed, takes no outline.
        for replace in [true, false] {
            let (mut app, loaded) = outlined(vec![chapter("Loaded", 1, 0)]);
            press_code(&mut app, KeyCode::Tab);
            if replace {
                deliver(&mut app, 0, "/books/fake.cbz", Ok(fake(3, "/books/fake.cbz")));
                assert!(app.menu.is_none(), "the menu of the replaced book closes");
            } else {
                app.close_pane(0);
            }
            app.on_event(loaded);
            assert!(app.panes[0].chapters.is_none(), "replace {replace}");
        }
    }

    #[test]
    fn empty_or_failed_outlines_say_so() {
        for result in [Ok(Vec::new()), Err("pdftohtml is not on PATH".to_owned())] {
            let mut app = reader(3);
            keys(&mut app, "z");
            press_code(&mut app, KeyCode::Tab);
            let book = book_of(&app, 0);
            app.on_event(Event::Chapters(book, result.clone()));
            assert!(app.menu.is_none() && !app.zen);
            let want = result.err().map_or("no chapter info in this book".to_owned(), |error| format!("chapters: {error}"));
            assert!(app.status_left().ends_with(&want), "{}", app.status_left());
        }
    }

    #[test]
    fn the_title_shows_the_current_chapter() {
        let outline = vec![chapter("First chapter", 1, 0), chapter("Second\n chapter", 2, 0), chapter(" \t ", 3, 0)];
        let mut app = reader(10);
        app.panes[0].chapters = Some(Ok(outline));
        app.panes[0].loading = false;
        for (page, spread, want) in [
            (0, false, "Fake  1/10"),
            (1, false, "Fake  2/10  [First chapter]"),
            (2, false, "Fake  3/10  [Second chapter]"),
            (1, true, "Fake  2-3/10  [First chapter]"),
            (3, false, "Fake  4/10"),
        ] {
            app.panes[0].page = page;
            app.spread = spread;
            assert_eq!(app.title(0), want);
        }
        app.panes[0].page = 1;
        app.spread = false;
        (app.panes[0].quarter_turns, app.panes[0].zoom) = (1, 1.5);
        assert_eq!(app.title(0), "Fake  2/10  [First chapter 90° 1.5x]");

        let outline = vec![chapter("Later", 2, 0), chapter("Earlier", 0, 0), chapter("Nested\x1b[31m title", 1, 2), chapter("Appendix", 0, 0)];
        app.panes[0].chapters = Some(Ok(outline));
        app.panes[0].reset_view();
        app.panes[0].quarter_turns = 0;
        assert_eq!(app.title(0), "Fake  2/10  [Nested title]", "the outline need not be sorted by page");
    }
}
