//! The webtoon strip: pages one under another, each scaled once to the
//! viewport width, sent to kitty as bands the terminal keeps while the reader
//! scrolls.

mod layout;
mod scaled;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;
use std::sync::Arc;

use crossbeam_channel::Sender;
use yoyomi_core::{At, Book, Cancel, Error, Frame, Grid, ImageId, Jobs, Layer, PageCache, Priority, Renderer, RendererKind, Result, Scene, Upload};
use yoyomi_render::MAX_CELLS;

use self::layout::Layout;
use self::scaled::{Key, Piece, ScaledPages, compose};
use crate::{Lease, Resident, Shown, Update, ViewEvent, next_source};

/// How the strip shows its pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StripRequest {
    /// The viewport.
    pub grid: Grid,
    /// Clockwise quarter turns of every page.
    pub quarter_turns: u8,
    /// Invert the pages the book allows.
    pub invert: bool,
}

/// A place in the strip: a page, and the fraction of it above the top of the
/// viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StripPosition {
    pub page: usize,
    pub offset: f64,
}

/// Shows one book as a continuous vertical strip.
pub struct Strip {
    cx: Context,
    /// Counts anchorings, so results for an older one are dropped.
    generation: u64,
    /// A `show` whose anchor page waits for its size, with the rows of
    /// scrolling asked for meanwhile and the fraction of a row kept for the
    /// next scroll.
    anchor: Option<(StripPosition, StripRequest, f64, f64)>,
    view: Option<View>,
    /// Under tmux, the ids of the bands a dropped view drew, held until the
    /// next frame shows: after a window resize kitty leaves placeholder cells
    /// blank when their image is uploaded again under the same id and the
    /// cells stay unchanged, so a render never reuses an id the screen shows.
    retired: Vec<Lease>,
}

/// What the strip shares with its jobs.
struct Context {
    source: u64,
    book: Arc<PageCache>,
    renderer: Arc<dyn Renderer>,
    jobs: Jobs,
    resident: Arc<Resident>,
    events: Sender<ViewEvent>,
    scaled: Arc<ScaledPages>,
    /// Page sizes the size jobs found; `None` for a page whose size did not
    /// load, kept until the view drops.
    sizes: HashMap<usize, Option<(u32, u32)>>,
    /// Pages a size job is running for.
    sizing: HashSet<usize>,
    /// Stops the size jobs.
    sizing_cancel: Cancel,
}

/// The strip from one `show` on: its geometry, layout and scrolling. Strip
/// pixels are terminal pixels, or for half blocks one pixel per cell across.
struct View {
    request: StripRequest,
    generation: u64,
    kitty: bool,
    /// Pixels for a native window, which eases with a velocity.
    native: bool,
    cols: u16,
    rows: u16,
    /// Strip pixels per terminal row.
    cell_h: f64,
    unit: f64,
    /// The viewport in strip pixels.
    width: u32,
    height: u32,
    layout: Layout,
    /// The strip row at the top of the viewport.
    top: f64,
    /// Terminal rows of scrolling still to run.
    pending: f64,
    /// Kitty: the fraction of a row the last scroll could not show.
    carry: f64,
    /// Native windows: the rows the last frame moved, which the next one
    /// builds on.
    velocity: f64,
    /// The viewport waits for page sizes or its frame; scrolling holds until
    /// they arrive.
    waiting: bool,
    /// Sixel and half blocks: the job composing the latest frame, and the
    /// count of frame jobs started, which names the latest.
    frame: Option<Cancel>,
    frame_job: u64,
    /// Kitty: the strip row band 0 starts at, the rows per band and the
    /// bands around the viewport by index.
    origin: f64,
    band_rows: i64,
    bands: HashMap<i64, Band>,
}

/// A kitty band: `band_rows` terminal rows of the strip as one image.
enum Band {
    Rendering { lease: Lease, cancel: Cancel },
    Ready { lease: Lease, frame: Frame, errors: Vec<String> },
    Failed(String),
}

impl Band {
    /// Stops its job and gives its id back.
    fn release(&self, resident: &Resident) {
        match self {
            Band::Rendering { lease, cancel } => {
                cancel.cancel();
                resident.release(*lease);
            }
            Band::Ready { lease, .. } => resident.release(*lease),
            Band::Failed(_) => {}
        }
    }
}

/// A finished job of the strip.
enum Done {
    Size { page: usize, result: Result<(u32, u32)> },
    Band { generation: u64, index: i64, lease: Lease, result: Result<(Frame, Vec<String>)> },
    Frame { generation: u64, job: u64, page: usize, result: Result<(Frame, Vec<String>)> },
}

impl Strip {
    /// A strip for `book`. Its jobs run on `jobs` and report through
    /// `events`; kitty bands take their ids from `resident`.
    pub fn new(book: Arc<PageCache>, renderer: Arc<dyn Renderer>, jobs: Jobs, resident: Arc<Resident>, events: Sender<ViewEvent>) -> Self {
        let scaled = Arc::new(ScaledPages::default());
        let (sizes, sizing, sizing_cancel) = (HashMap::new(), HashSet::new(), Cancel::new());
        let cx = Context { source: next_source(), book, renderer, jobs, resident, events, scaled, sizes, sizing, sizing_cancel };
        Strip { cx, generation: 0, anchor: None, view: None, retired: Vec::new() }
    }

    /// Shows the strip from `at` with `request`, anchoring it at that page.
    pub fn show(&mut self, at: StripPosition, request: StripRequest) -> Update {
        self.drop_view();
        let count = self.cx.book.len();
        if count == 0 {
            return Update::default();
        }
        self.generation += 1;
        let offset = if at.offset.is_nan() { 0.0 } else { at.offset.clamp(0.0, 1.0) };
        self.anchor = Some((StripPosition { page: at.page.min(count - 1), offset }, request, 0.0, 0.0));
        self.anchor_view()
    }

    /// Builds the view of the latest `show` once its anchor page's size is
    /// known.
    fn anchor_view(&mut self) -> Update {
        let Some((at, request, pending, carry)) = self.anchor.take() else { return Update::default() };
        let Some(size) = self.cx.size(at.page) else {
            self.anchor = Some((at, request, pending, carry));
            return Update::default();
        };
        let renderer = &self.cx.renderer;
        let kitty = renderer.kind() == RendererKind::Kitty;
        let cell = renderer.cell_size();
        // A half-block cell shows one pixel across, so the strip composes no
        // finer than that.
        let unit = if renderer.kind() == RendererKind::HalfBlock { cell.w.max(1.0) } else { 1.0 };
        let (cell_w, cell_h) = (cell.w.max(1.0) / unit, cell.h.max(1.0) / unit);
        let cols = if kitty { request.grid.cols.clamp(1, MAX_CELLS) } else { request.grid.cols.max(1) };
        let rows = request.grid.rows.max(1);
        // Kitty bands no wider than their cells keep every column in fit_box.
        let width = f64::from(cols) * cell_w;
        let width = (if kitty { width.floor() } else { width.round() } as u32).max(1);
        // A coarse frame rounded up in height would fit its cells by height
        // and lose a column; rounded down, the width decides.
        let height = f64::from(rows) * cell_h;
        let height = (if unit > 1.0 { height.floor() } else { height.round() } as u32).max(1);
        let size = scaled_size(size, request.quarter_turns, unit, width, height);
        let mut view = View {
            band_rows: i64::from(rows.min(MAX_CELLS)),
            request,
            generation: self.generation,
            kitty,
            native: renderer.kind() == RendererKind::Native,
            cols,
            rows,
            cell_h,
            unit,
            width,
            height,
            layout: Layout::new(at.page, self.cx.book.len(), size),
            top: at.offset * f64::from(size.1),
            pending,
            carry,
            velocity: 0.0,
            waiting: false,
            frame: None,
            frame_job: 0,
            origin: 0.0,
            bands: HashMap::new(),
        };
        view.clamp(&mut self.cx);
        view.origin = view.top;
        let update = view.refresh(&mut self.cx);
        self.view = Some(view);
        self.release_retired(update)
    }

    /// Gives the held ids back once `update` shows a frame without them.
    fn release_retired(&mut self, update: Update) -> Update {
        if update.shown.is_some() {
            for lease in self.retired.drain(..) {
                self.cx.resident.release(lease);
            }
        }
        update
    }

    /// Adds `rows` terminal rows to the scrolling still to run; `tick` eases
    /// toward it. Before the anchor page's size arrives, the rows wait for
    /// the view.
    pub fn scroll(&mut self, rows: f64) {
        if !rows.is_finite() {
            return;
        }
        // Turning around drops what was left of the other direction.
        match (&mut self.view, &mut self.anchor) {
            (Some(view), _) => {
                if view.pending * rows < 0.0 || view.velocity * rows < 0.0 {
                    view.pending = 0.0;
                    view.velocity = 0.0;
                }
                view.pending += rows + std::mem::take(&mut view.carry);
            }
            (None, Some((_, _, pending, carry))) => {
                *pending = if *pending * rows < 0.0 { rows } else { *pending + rows } + std::mem::take(carry);
            }
            (None, None) => {}
        }
    }

    /// Kitty: the fraction of a row the last scroll could not show, which
    /// the next scroll adds.
    pub fn carry(&self) -> f64 {
        match (&self.view, &self.anchor) {
            (Some(view), _) => view.carry,
            (None, Some((.., carry))) => *carry,
            (None, None) => 0.0,
        }
    }

    /// Keeps `carry`, from `carry` before the strip showed again, for the
    /// next scroll. The view stays where it is.
    pub fn keep_carry(&mut self, carry: f64) {
        match (&mut self.view, &mut self.anchor) {
            (Some(view), _) => view.carry = carry,
            (None, Some((.., kept))) => *kept = carry,
            (None, None) => {}
        }
    }

    /// Terminal rows of scrolling still to run.
    pub fn pending(&self) -> f64 {
        match (&self.view, &self.anchor) {
            (Some(view), _) => view.pending,
            (None, Some((_, _, pending, _))) => *pending,
            (None, None) => 0.0,
        }
    }

    /// Runs one frame of eased scrolling.
    pub fn tick(&mut self) -> Update {
        let Some(view) = &mut self.view else { return Update::default() };
        if view.waiting || view.pending == 0.0 {
            return Update::default();
        }
        let pending = view.pending;
        let rows = if view.native {
            // The native window eases: the velocity follows the distance
            // left, at most 64 pixels a frame, and never overshoots.
            view.velocity = (view.velocity + pending * 0.075) * 0.8;
            let step = view.velocity.clamp(-64.0, 64.0);
            if step.abs() > pending.abs() { pending } else { step }
        } else if view.kitty {
            // Placeholder cells move in whole rows.
            let rows = (pending * 0.3).round().clamp(-3.0, 3.0);
            if rows == 0.0 { pending.round() } else { rows }
        } else if pending.abs() < 0.02 {
            pending
        } else {
            (pending * 0.3).clamp(-3.0, 3.0)
        };
        view.pending -= rows;
        // Half a row left would round to a whole row each way and never stop;
        // the next scroll takes it, so two half viewports make a whole one.
        if view.kitty && view.pending.abs() <= 0.5 {
            view.carry = view.pending;
            view.pending = 0.0;
        }
        if view.native && view.pending.abs() < 0.05 {
            view.pending = 0.0;
            view.velocity = 0.0;
        }
        let before = view.top;
        let target = before + rows * view.cell_h;
        view.top = target;
        view.clamp(&mut self.cx);
        if (view.top - target).abs() > 1e-9 {
            // An end of the strip stops the scrolling.
            view.pending = 0.0;
            view.velocity = 0.0;
        }
        // At an end, clamped or landed on, there is no half row to keep.
        if view.at_end() {
            view.carry = 0.0;
        }
        if (view.top - before).abs() < 1e-9 {
            return Update::default();
        }
        if view.kitty {
            view.align(&self.cx.resident);
        }
        let update = view.refresh(&mut self.cx);
        self.release_retired(update)
    }

    /// Whether scrolling still eases, so the app keeps calling `tick`.
    pub fn moving(&self) -> bool {
        self.view.as_ref().is_some_and(|view| view.pending != 0.0)
    }

    /// Where the top of the viewport is.
    pub fn position(&self) -> StripPosition {
        let anchor = self.anchor.as_ref().map(|(at, ..)| *at);
        self.view.as_ref().map(View::position).or(anchor).unwrap_or_default()
    }

    /// The page under point `x`, `y` of the viewport, in strip pixels, with
    /// the point as fractions of the turned page. Margins beside narrow pages
    /// and the space past the strip's end have none.
    pub fn page_at(&self, x: f64, y: f64) -> Option<(usize, f64, f64)> {
        let view = self.view.as_ref()?;
        let y = view.top.round() + y;
        let placed = view.layout.page_at(y);
        let left = f64::from(view.width.saturating_sub(placed.width) / 2);
        let (fx, fy) = ((x - left) / f64::from(placed.width), (y - placed.top as f64) / f64::from(placed.height));
        ((0.0..1.0).contains(&fx) && (0.0..1.0).contains(&fy)).then_some((placed.page, fx, fy))
    }

    /// Takes a finished job of this strip, or hands back an event it did not
    /// start.
    pub fn on_event(&mut self, event: ViewEvent) -> Result<Update, ViewEvent> {
        let done = event.take::<Done>(self.cx.source)?;
        if let Done::Size { page, result } = &done {
            self.cx.sizing.remove(page);
            // A cancelled job found nothing; the next request asks again.
            if !result.as_ref().is_err_and(Error::is_cancelled) {
                self.cx.sizes.insert(*page, result.as_ref().ok().copied().filter(|&(w, h)| w > 0 && h > 0));
            }
            if self.anchor.is_some() {
                return Ok(self.anchor_view());
            }
        }
        let update = self.view.as_mut().map(|view| view.finish(&mut self.cx, done)).unwrap_or_default();
        Ok(self.release_retired(update))
    }

    /// Drops every band and scaled page the strip keeps and gives the band
    /// ids back to the `Resident`; under tmux the ids of ready bands come
    /// back with the next frame.
    pub fn clear(&mut self) {
        self.drop_view();
        self.generation += 1;
        self.cx.scaled.clear();
        self.cx.sizing_cancel.cancel();
        self.cx.sizing_cancel = Cancel::new();
        self.cx.sizing.clear();
    }

    /// Stops the view's jobs and gives its band ids back. Pages whose size did
    /// not load ask again in the next view.
    fn drop_view(&mut self) {
        self.anchor = None;
        self.cx.sizes.retain(|_, size| size.is_some());
        if let Some(view) = self.view.take() {
            if let Some(cancel) = view.frame {
                cancel.cancel();
            }
            for band in view.bands.values() {
                match band {
                    Band::Ready { lease, .. } if self.cx.resident.resend() => self.retired.push(*lease),
                    band => band.release(&self.cx.resident),
                }
            }
        }
    }
}

impl Drop for Strip {
    fn drop(&mut self) {
        self.drop_view();
        for lease in self.retired.drain(..) {
            self.cx.resident.release(lease);
        }
        self.cx.sizing_cancel.cancel();
    }
}

/// A page of `size` in the strip: turned, shrunk to `width` when wider and
/// to the strip's pixel unit. A page whose size did not load takes the
/// viewport's size, and its decode reports the error.
fn scaled_size(size: Option<(u32, u32)>, quarter_turns: u8, unit: f64, width: u32, height: u32) -> (u32, u32) {
    let Some((w, h)) = size else { return (width, height) };
    let (w, h) = if quarter_turns & 1 == 1 { (h, w) } else { (w, h) };
    let scale = (1.0 / unit).min(f64::from(width) / f64::from(w));
    let fit = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    (fit(w), fit(h))
}

impl View {
    /// Places the pages that strip rows `y0..y1` cover, as far as their sizes
    /// are known; the others start size jobs.
    fn reach(&mut self, cx: &mut Context, y0: f64, y1: f64) {
        let (turns, unit, width, height) = (self.request.quarter_turns, self.unit, self.width, self.height);
        self.layout.reach(y0.floor() as i64, y1.ceil() as i64, |page| cx.size(page).map(|size| scaled_size(size, turns, unit, width, height)));
    }

    /// Keeps the viewport below the top of page 0 and the end of the strip
    /// at or below its bottom, as far as the placed pages show them.
    fn clamp(&mut self, cx: &mut Context) {
        let height = f64::from(self.height);
        self.reach(cx, self.top, self.top + height);
        if let Some(start) = self.layout.start() {
            self.top = self.top.max(start as f64);
        }
        self.reach(cx, self.top, self.top + height);
        if let Some(end) = self.layout.end()
            && self.top + height > end as f64
        {
            self.top = end as f64 - height;
            self.reach(cx, self.top, self.top + height);
            if let Some(start) = self.layout.start() {
                self.top = self.top.max(start as f64);
            }
        }
    }

    /// Whether the viewport touches the top or the bottom of the strip.
    fn at_end(&self) -> bool {
        self.layout.start().is_some_and(|start| self.top <= start as f64 + 1e-9)
            || self.layout.end().is_some_and(|end| self.top + f64::from(self.height) >= end as f64 - 1e-9)
    }

    fn position(&self) -> StripPosition {
        let placed = self.layout.page_at(self.top);
        StripPosition { page: placed.page, offset: (self.top - placed.top as f64) / f64::from(placed.height) }
    }

    /// The pieces of the pages in strip rows `y..y + height`, placed from
    /// row `y` on.
    fn pieces(&self, book: &PageCache, y: i64, height: u32) -> Vec<Piece> {
        let StripRequest { quarter_turns, invert, .. } = self.request;
        self.layout
            .overlapping(y, y + i64::from(height))
            .map(|placed| {
                let (page, width, height) = (placed.page, placed.width, placed.height);
                let key = Key { page, quarter_turns, width, height, invert: invert && book.can_invert(page) };
                Piece { key, x: self.width.saturating_sub(placed.width) / 2, y: placed.top - y }
            })
            .collect()
    }

    /// Asks for the viewport's frame: at once when its bands are ready,
    /// otherwise through `finish`.
    fn refresh(&mut self, cx: &mut Context) -> Update {
        if !self.kitty {
            let (top, height) = (self.top, f64::from(self.height));
            // Sizes a viewport above and below load ahead of the scrolling.
            self.reach(cx, top - height, top + 2.0 * height);
            let y = top.round() as i64;
            if !self.layout.covers(y, y + i64::from(self.height)) {
                if let Some(cancel) = self.frame.take() {
                    cancel.cancel();
                }
                self.waiting = true;
                return Update::default();
            }
            return self.compose_frame(cx);
        }
        self.request_bands(cx);
        let mut update = Update::default();
        if self.bands_ready() {
            update.shown = Some(self.band_scene(cx, &mut update.uploads));
        } else {
            self.waiting = true;
        }
        update
    }

    /// Starts rendering the wanted bands the layout covers, and drops the
    /// others.
    fn request_bands(&mut self, cx: &mut Context) {
        let (visible, wanted) = self.band_ranges();
        let (y0, _) = self.band_span(*wanted.start());
        let (y1, height) = self.band_span(*wanted.end());
        self.reach(cx, y0 as f64, (y1 + i64::from(height)) as f64);
        // Bands whose id went to another image render again.
        self.bands.retain(|index, band| {
            let keep = wanted.contains(index) && !matches!(band, Band::Ready { lease, .. } if !cx.resident.holds(*lease));
            if !keep {
                band.release(&cx.resident);
            }
            keep
        });
        for index in wanted {
            if !self.bands.contains_key(&index) && self.covers_band(index) {
                let priority = if visible.contains(&index) { Priority::Visible } else { Priority::Neighbor };
                self.render_band(cx, index, priority);
            }
        }
    }

    /// Whether the layout holds every page of band `index`.
    fn covers_band(&self, index: i64) -> bool {
        let (y, height) = self.band_span(index);
        self.layout.covers(y, y + i64::from(height))
    }

    /// The viewport's row in the band grid.
    fn top_row(&self) -> i64 {
        ((self.top - self.origin) / self.cell_h).round() as i64
    }

    /// The bands the viewport shows, and those it keeps: the visible ones
    /// with one band above and one below the band at the top.
    fn band_ranges(&self) -> (RangeInclusive<i64>, RangeInclusive<i64>) {
        let n = self.top_row();
        let (first, last) = (n.div_euclid(self.band_rows), (n + i64::from(self.rows) - 1).div_euclid(self.band_rows));
        (first..=last, first - 1..=last.max(first + 1))
    }

    /// The first strip row of band `index` and its height in strip pixels.
    /// Each band ends where the next starts, so the bands tile the strip.
    fn band_span(&self, index: i64) -> (i64, u32) {
        let start = |index: i64| (self.origin + (index * self.band_rows) as f64 * self.cell_h).round() as i64;
        (start(index), (start(index + 1) - start(index)) as u32)
    }

    /// Starts rendering band `index`, unless no page reaches into it.
    fn render_band(&mut self, cx: &Context, index: i64, priority: Priority) {
        let (y, height) = self.band_span(index);
        let pieces = self.pieces(&cx.book, y, height);
        if pieces.is_empty() {
            return;
        }
        let lease = cx.resident.lease();
        let cancel = Cancel::new();
        let generation = self.generation;
        let grid = Grid::new(self.cols, self.band_rows as u16);
        cx.render(priority, &cancel, pieces, (self.width, height), lease.id, grid, move |result| Done::Band { generation, index, lease, result });
        self.bands.insert(index, Band::Rendering { lease, cancel });
    }

    /// Whether every visible band that holds pages is ready or failed.
    fn bands_ready(&self) -> bool {
        self.band_ranges().0.all(|index| match self.bands.get(&index) {
            Some(Band::Rendering { .. }) => false,
            Some(_) => true,
            None => self.covers_band(index),
        })
    }

    /// The viewport as rows of the visible bands. Under tmux, `uploads` takes
    /// the upload of every band the scene draws.
    fn band_scene(&mut self, cx: &Context, uploads: &mut Vec<Upload>) -> Shown {
        let (n, rows, band_rows) = (self.top_row(), i64::from(self.rows), self.band_rows);
        let (mut layers, mut errors) = (Vec::new(), Vec::new());
        for index in self.band_ranges().0 {
            match self.bands.get_mut(&index) {
                Some(Band::Ready { lease, frame, errors: failed }) => {
                    // Placeholder rows past the grid the renderer returned
                    // would point outside the image.
                    let grid = frame.grid();
                    let from = n.max(index * band_rows);
                    let to = (n + rows).min(index * band_rows + i64::from(grid.rows));
                    if to > from {
                        let at = At { col: 0, row: (from - n) as u16 };
                        let (src_row, rows) = ((from - index * band_rows) as u16, (to - from) as u16);
                        layers.push(Layer::KittyRows { id: lease.id, cols: grid.cols, src_row, rows, at });
                        if cx.resident.resend() {
                            uploads.extend(Upload::from_frame(frame));
                        }
                    }
                    cx.resident.touch(*lease);
                    errors.extend(failed.iter().cloned());
                }
                Some(Band::Failed(message)) => errors.push(message.clone()),
                _ => {}
            }
        }
        errors.sort();
        errors.dedup();
        self.waiting = false;
        Shown { page: self.position().page, scene: Scene { layers }, errors }
    }

    /// Kitty scrolls in whole rows of the band grid. When an end of the strip
    /// stops the viewport between rows, the grid starts again there.
    fn align(&mut self, resident: &Resident) {
        let rows = (self.top - self.origin) / self.cell_h;
        if (rows - rows.round()).abs() > 1e-6 {
            self.origin = self.top;
            for (_, band) in self.bands.drain() {
                band.release(resident);
            }
        } else {
            self.top = self.origin + rows.round() * self.cell_h;
        }
    }

    /// Sixel and half blocks: starts composing the viewport from the scaled
    /// pages and rendering it.
    fn compose_frame(&mut self, cx: &Context) -> Update {
        if let Some(cancel) = self.frame.take() {
            cancel.cancel();
        }
        let pieces = self.pieces(&cx.book, self.top.round() as i64, self.height);
        let cancel = Cancel::new();
        self.frame_job += 1;
        let (generation, job, page) = (self.generation, self.frame_job, self.position().page);
        let (grid, done) = (Grid::new(self.cols, self.rows), move |result| Done::Frame { generation, job, page, result });
        cx.render(Priority::Visible, &cancel, pieces, (self.width, self.height), ImageId(0), grid, done);
        self.frame = Some(cancel);
        self.waiting = true;
        Update::default()
    }

    /// Places the pages a size job found, and asks for the frame that waited
    /// for them.
    fn sized(&mut self, cx: &mut Context) -> Update {
        let before = self.top;
        self.clamp(cx);
        let moved = (self.top - before).abs() > 1e-9;
        if moved {
            // An end of the strip turned up inside the viewport.
            self.pending = 0.0;
            self.velocity = 0.0;
            self.carry = 0.0;
            if self.kitty {
                self.align(&cx.resident);
            }
        }
        if moved || (self.waiting && self.frame.is_none()) {
            return self.refresh(cx);
        }
        if self.kitty {
            self.request_bands(cx);
        }
        Update::default()
    }

    /// Takes a finished job of this view.
    fn finish(&mut self, cx: &mut Context, done: Done) -> Update {
        let mut update = Update::default();
        match done {
            Done::Size { .. } => return self.sized(cx),
            Done::Band { generation, index, lease, result } => {
                let current = matches!(self.bands.get(&index), Some(Band::Rendering { lease: running, .. }) if *running == lease);
                if generation != self.generation || !current {
                    return update;
                }
                let band = match result {
                    Ok((frame, errors)) => {
                        // Under tmux a band goes out with the frames that draw it.
                        if !cx.resident.resend() {
                            update.uploads.extend(Upload::from_frame(&frame));
                        }
                        Band::Ready { lease, frame, errors }
                    }
                    Err(error) => {
                        cx.resident.release(lease);
                        Band::Failed(error.to_string())
                    }
                };
                self.bands.insert(index, band);
                if self.waiting && self.bands_ready() {
                    update.shown = Some(self.band_scene(cx, &mut update.uploads));
                }
            }
            Done::Frame { generation, job, page, result } => {
                // A replaced job's result may already wait in the queue.
                if generation != self.generation || job != self.frame_job || self.frame.is_none() {
                    return update;
                }
                self.frame = None;
                self.waiting = false;
                update.shown = Some(match result {
                    Ok((frame, errors)) => Shown { page, scene: Scene { layers: vec![Layer::from_frame(&frame, At::default())] }, errors },
                    Err(error) => Shown { page, scene: Scene::default(), errors: vec![error.to_string()] },
                });
            }
        }
        update
    }
}

impl Context {
    /// The size of `page` once a size job found it, `Some(None)` when it did
    /// not load. An unknown size starts a job, which reports to `on_event`.
    fn size(&mut self, page: usize) -> Option<Option<(u32, u32)>> {
        if let Some(&size) = self.sizes.get(&page) {
            return Some(size);
        }
        if self.sizing.insert(page) {
            let (book, events, source, cancel) = (self.book.clone(), self.events.clone(), self.source, self.sizing_cancel.clone());
            self.jobs.spawn(Priority::Visible, &self.sizing_cancel, move || {
                // A page whose size lookup fails may still decode at its own shape.
                let result = book
                    .page_size(page, &cancel)
                    .or_else(|error| if error.is_cancelled() { Err(error) } else { book.page(page, &cancel).map(|pixels| pixels.dimensions()) });
                // The app may have dropped the receiver while it shuts down.
                let _ = events.send(ViewEvent::new(source, Done::Size { page, result }));
            });
        }
        None
    }

    /// Composes `pieces` into an image of `size` and renders it into `grid`
    /// on a job; `done` wraps the result for `events`.
    #[allow(clippy::too_many_arguments, reason = "one call site per strip mode passes the job's whole input")]
    fn render(
        &self,
        priority: Priority,
        cancel: &Cancel,
        pieces: Vec<Piece>,
        (width, height): (u32, u32),
        id: ImageId,
        grid: Grid,
        done: impl FnOnce(Result<(Frame, Vec<String>)>) -> Done + Send + 'static,
    ) {
        let (book, scaled, renderer, events, source) =
            (self.book.clone(), self.scaled.clone(), self.renderer.clone(), self.events.clone(), self.source);
        let job_cancel = cancel.clone();
        let refuse_text = renderer.kind() == RendererKind::HalfBlock;
        self.jobs.spawn(priority, cancel, move || {
            let result = compose(&book, &scaled, &pieces, (width, height), refuse_text, &job_cancel)
                .and_then(|(image, errors)| Ok((renderer.render(&image, id, grid, false)?, errors)));
            // The app may have dropped the receiver while it shuts down.
            let _ = events.send(ViewEvent::new(source, done(result)));
        });
    }
}
