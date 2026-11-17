use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use parking_lot::Mutex;
use yoyomi_core::image::Rgba;
use yoyomi_core::{CellSize, Error, ImageIds, PageKind, Pixels, RgbaImage, fit_box};
use yoyomi_render::{HalfBlock, Kitty, invert, rotate, scale};

use super::scaled::{SECTION_ROWS, Section, TEXT_NEEDS_PIXELS};
use super::*;

const CELL: CellSize = CellSize { w: 8.0, h: 16.0 };
const PIXEL: CellSize = CellSize { w: 1.0, h: 1.0 };
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// A book of fixed pages that counts its decodes and notes the threads that
/// read it. Page `broken` fails to decode, slowly; the size lookup of page
/// `sizeless` fails; page `text` is laid out from text.
struct Pages {
    path: PathBuf,
    pages: Vec<Pixels>,
    invertible: Vec<bool>,
    broken: Option<usize>,
    sizeless: Option<usize>,
    text: Option<usize>,
    decodes: AtomicUsize,
    readers: Mutex<Vec<thread::ThreadId>>,
}

impl Pages {
    fn new(pages: Vec<RgbaImage>) -> Arc<Self> {
        let invertible = vec![true; pages.len()];
        Self::invertible(pages, invertible)
    }

    fn invertible(pages: Vec<RgbaImage>, invertible: Vec<bool>) -> Arc<Self> {
        let pages = pages.into_iter().map(Arc::new).collect();
        let (path, decodes, readers) = (PathBuf::from("/books/strip.cbz"), AtomicUsize::new(0), Mutex::new(Vec::new()));
        Arc::new(Pages { path, pages, invertible, broken: None, sizeless: None, text: None, decodes, readers })
    }

    fn broken(pages: Vec<RgbaImage>, broken: usize) -> Arc<Self> {
        let mut book = Arc::into_inner(Self::new(pages)).expect("a new book");
        book.broken = Some(broken);
        Arc::new(book)
    }

    fn sizeless(pages: Vec<RgbaImage>, sizeless: usize, broken: Option<usize>) -> Arc<Self> {
        let mut book = Arc::into_inner(Self::new(pages)).expect("a new book");
        (book.sizeless, book.broken) = (Some(sizeless), broken);
        Arc::new(book)
    }
}

impl Book for Pages {
    fn path(&self) -> &Path {
        &self.path
    }

    fn title(&self) -> &str {
        "strip"
    }

    fn len(&self) -> usize {
        self.pages.len()
    }

    fn kind(&self, page: usize) -> PageKind {
        if self.text == Some(page) { PageKind::Text } else { PageKind::Image }
    }

    fn can_invert(&self, page: usize) -> bool {
        self.invertible[page]
    }

    fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
        cancel.check()?;
        self.readers.lock().push(thread::current().id());
        self.decodes.fetch_add(1, Ordering::SeqCst);
        if self.broken == Some(page) {
            thread::sleep(Duration::from_millis(100));
            return Err(Error::Tool { tool: "pdftoppm".into(), message: "broken page".into() });
        }
        Ok(self.pages[page].clone())
    }

    fn page_size(&self, page: usize, _cancel: &Cancel) -> Result<(u32, u32)> {
        self.readers.lock().push(thread::current().id());
        if self.sizeless == Some(page) {
            return Err(Error::Tool { tool: "pdfinfo".into(), message: "no size".into() });
        }
        Ok(self.pages[page].dimensions())
    }
}

/// A page whose red channel names it and whose green channel counts its
/// rows, as testdata/webtoon.cbz has.
fn rows_page(page: usize, width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |_, y| Rgba([page as u8, (y % 256) as u8, 0, 255]))
}

/// A page whose every row and column has its own color.
fn varied_page(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| Rgba([(x * 13 + y) as u8, (y * 7 + x) as u8, (x * 5 + y * 3) as u8, 255]))
}

/// Checks that row `y` of `got` matches row `want_y` of `want` to within one
/// level per channel.
fn assert_row_near(got: &RgbaImage, y: u32, want: &RgbaImage, want_y: u32, what: &str) {
    assert_eq!(got.width(), want.width(), "{what}");
    for x in 0..got.width() {
        let (a, b) = (pixel(got, x, y), pixel(want, x, want_y));
        assert!(a.iter().zip(b).all(|(&a, b)| a.abs_diff(b) <= 1), "{what}: pixel {x},{y} is {a:?}, want {b:?}");
    }
}

fn solid(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(width, height, Rgba(color))
}

/// Keeps every image it renders and answers with a small frame of its kind.
struct Recorder {
    kind: RendererKind,
    cell: CellSize,
    images: Mutex<Vec<(ImageId, RgbaImage)>>,
}

impl Recorder {
    fn new(kind: RendererKind, cell: CellSize) -> Arc<Self> {
        Arc::new(Recorder { kind, cell, images: Mutex::new(Vec::new()) })
    }

    fn renders(&self) -> usize {
        self.images.lock().len()
    }

    fn last(&self) -> RgbaImage {
        self.images.lock().last().expect("a render").1.clone()
    }

    /// The last image rendered under `id`.
    fn image(&self, id: ImageId) -> RgbaImage {
        self.images.lock().iter().rev().find(|(drawn, _)| *drawn == id).expect("a render under the id").1.clone()
    }
}

impl Renderer for Recorder {
    fn kind(&self) -> RendererKind {
        self.kind
    }

    fn cell_size(&self) -> CellSize {
        self.cell
    }

    fn set_cell_size(&self, _cell: CellSize) {}

    fn render(&self, image: &RgbaImage, id: ImageId, max: Grid, _invert: bool) -> Result<Frame> {
        let max = Grid::new(max.cols.min(MAX_CELLS), max.rows.min(MAX_CELLS));
        let grid = fit_box(image.width(), image.height(), max, self.cell).grid;
        self.images.lock().push((id, image.clone()));
        Ok(match self.kind {
            RendererKind::Kitty => Frame::Kitty { id, grid, upload: format!("image {}", id.0).into_bytes().into() },
            RendererKind::Sixel => Frame::Sixel { grid, data: Arc::from(&b"sixel"[..]) },
            _ => Frame::Cells { grid, colors: Arc::from(Vec::new()) },
        })
    }

    fn delete(&self, _id: ImageId) -> Vec<u8> {
        Vec::new()
    }
}

struct Harness {
    strip: Strip,
    events: Receiver<ViewEvent>,
    resident: Arc<Resident>,
}

fn harness(book: Arc<Pages>, renderer: Arc<dyn Renderer>, resend: bool) -> Harness {
    let (sender, events) = unbounded();
    let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), resend));
    let strip = Strip::new(Arc::new(PageCache::new(book)), renderer, Jobs::new(2), resident.clone(), sender);
    Harness { strip, events, resident }
}

fn request(cols: u16, rows: u16) -> StripRequest {
    StripRequest { grid: Grid::new(cols, rows), quarter_turns: 0, invert: false, fill: false }
}

impl Harness {
    fn show(&mut self, page: usize, offset: f64, request: StripRequest) -> Update {
        let update = self.strip.show(StripPosition { page, offset }, request);
        self.settle(update)
    }

    /// Hands finished jobs to the strip until none runs, adding their
    /// updates to `update`.
    fn settle(&mut self, mut update: Update) -> Update {
        for events in 0.. {
            if !self.busy() {
                break;
            }
            assert!(events < 1000, "the jobs settle");
            let event = self.events.recv_timeout(Duration::from_secs(10)).expect("a job finishes");
            let next = self.strip.on_event(event).expect("the strip started the job");
            update.uploads.extend(next.uploads);
            if next.shown.is_some() {
                update.shown = next.shown;
            }
        }
        update
    }

    fn busy(&self) -> bool {
        let cx = &self.strip.cx;
        self.strip.anchor.is_some()
            || !cx.sizing.is_empty()
            || self.strip.view.as_ref().is_some_and(|view| view.waiting || view.bands.values().any(|band| matches!(band, Band::Rendering { .. })))
    }

    /// Scrolls `rows` and ticks every 10 ms of a made-up clock until the
    /// scrolling stops, settling every frame. Returns the update of every
    /// frame.
    fn scroll(&mut self, rows: f64) -> Vec<Update> {
        self.strip.scroll(rows);
        let start = Instant::now();
        let mut frames = Vec::new();
        while self.strip.moving() {
            let update = self.strip.tick(start + Duration::from_millis(10) * frames.len() as u32);
            frames.push(self.settle(update));
            assert!(frames.len() < 1000, "the scrolling stops");
        }
        frames
    }

    /// Runs the tick at `now` and settles its frame.
    fn tick(&mut self, now: Instant) {
        let update = self.strip.tick(now);
        self.settle(update);
    }

    fn view(&self) -> &View {
        self.strip.view.as_ref().expect("a shown strip")
    }

    fn band(&self, index: i64) -> Lease {
        match self.view().bands.get(&index) {
            Some(Band::Ready { lease, .. }) => *lease,
            _ => panic!("band {index} is not ready"),
        }
    }

    fn position(&self) -> (usize, f64) {
        let position = self.strip.position();
        (position.page, position.offset)
    }
}

/// The kitty layers of a frame as (id, src_row, rows, at row).
fn kitty_rows(update: &Update) -> Vec<(ImageId, u16, u16, u16)> {
    let shown = update.shown.as_ref().expect("a frame");
    shown
        .scene
        .layers
        .iter()
        .map(|layer| match layer {
            Layer::KittyRows { id, cols: 10, src_row, rows, at } => (*id, *src_row, *rows, at.row),
            other => panic!("not ten columns of kitty rows: {other:?}"),
        })
        .collect()
}

fn pixel(image: &RgbaImage, x: u32, y: u32) -> [u8; 4] {
    image.get_pixel(x, y).0
}

fn assert_position(harness: &Harness, page: usize, offset: f64) {
    let (got_page, got_offset) = harness.position();
    assert!(got_page == page && (got_offset - offset).abs() < 1e-9, "position page {got_page} offset {got_offset}, want page {page} offset {offset}");
}

#[test]
fn scrolling_within_resident_bands_renders_and_uploads_nothing() {
    let kitty = Recorder::new(RendererKind::Kitty, CELL);
    let mut h = harness(Pages::new((0..20).map(|i| rows_page(i, 80, 400)).collect()), kitty.clone(), false);
    let shown = h.show(0, 0.0, request(10, 10));
    // Band 0 shows and band 1 waits below it; nothing lies above page 0.
    assert_eq!(kitty.renders(), 2);
    assert_eq!(shown.uploads.len(), 2);
    let (band0, band1) = (h.band(0).id, h.band(1).id);
    assert_eq!(kitty_rows(&shown), [(band0, 0, 10, 0)]);

    let frames = h.scroll(5.0);
    assert!(frames.iter().all(|frame| frame.uploads.is_empty() && frame.shown.is_some()));
    assert_eq!(kitty_rows(frames.last().unwrap()), [(band0, 5, 5, 0), (band1, 0, 5, 5)]);
    let frames = h.scroll(-5.0);
    assert!(frames.iter().all(|frame| frame.uploads.is_empty()));
    assert_eq!(kitty_rows(frames.last().unwrap()), [(band0, 0, 10, 0)]);
    assert_eq!(kitty.renders(), 2, "scrolling inside the resident bands renders nothing");

    // Reaching band 1 readies band 2 below it, and nothing else.
    let frames = h.scroll(10.0);
    assert_eq!(kitty.renders(), 3);
    assert_eq!(frames.iter().map(|frame| frame.uploads.len()).sum::<usize>(), 1);
    assert_eq!(kitty_rows(frames.last().unwrap()), [(band1, 0, 10, 0)]);
}

#[test]
fn under_tmux_bands_go_out_with_the_frames_that_draw_them() {
    let kitty = Recorder::new(RendererKind::Kitty, CELL);
    let mut h = harness(Pages::new((0..4).map(|i| rows_page(i, 80, 400)).collect()), kitty, true);
    let shown = h.show(0, 0.0, request(10, 10));
    let (band0, band1) = (h.band(0).id, h.band(1).id);
    let ids = |update: &Update| update.uploads.iter().map(|upload| upload.id).collect::<Vec<_>>();
    assert_eq!(ids(&shown), [band0], "band 1 waits below the viewport and sends nothing ahead");
    let frames = h.scroll(1.0);
    assert_eq!(frames.len(), 1);
    assert_eq!(ids(&frames[0]), [band0, band1]);
    let Some(Band::Ready { frame, .. }) = h.view().bands.get(&0) else { panic!("band 0 is not ready") };
    let upload = Upload::from_frame(frame).expect("a kitty band");
    assert_eq!((frames[0].uploads[0].id, &frames[0].uploads[0].bytes), (upload.id, &upload.bytes));
}

#[test]
fn a_page_scales_once_per_width_and_turn() {
    // More pages than the page cache holds, so scaling again would also
    // decode again.
    let book = Pages::new((0..30).map(|i| rows_page(i, 160, 200)).collect());
    let kitty = Recorder::new(RendererKind::Kitty, CELL);
    let mut h = harness(book.clone(), kitty.clone(), false);
    h.show(0, 0.0, request(10, 10));
    // A viewport at a time, so every band on the way renders.
    for _ in 0..2 {
        for rows in [10.0; 6].into_iter().chain([-10.0; 6]) {
            h.scroll(rows);
        }
    }
    let scaled = h.strip.cx.scaled.clone();
    assert!(scaled.kept() > PageCache::MAX_PAGES);
    assert_eq!(scaled.scales(), scaled.kept());
    assert_eq!(book.decodes.load(Ordering::SeqCst), scaled.kept());
    // Eight bands reach down to row 70; scrolling back renders them again.
    assert!(kitty.renders() > 2 * 8, "bands rendered again from the scaled pages");

    let unturned = scaled.scales();
    h.show(0, 0.0, StripRequest { quarter_turns: 1, ..request(10, 10) });
    let after_turn = scaled.scales();
    assert!(after_turn > unturned, "a turn scales the pages again");
    h.show(0, 0.0, request(10, 10));
    assert_eq!(scaled.scales(), after_turn, "the unturned pages are still scaled");
}

#[test]
fn show_and_tick_leave_page_sizes_to_the_jobs() {
    for kind in [RendererKind::Kitty, RendererKind::Sixel, RendererKind::HalfBlock] {
        let book = Pages::new((0..10).map(|i| rows_page(i, 80, 100)).collect());
        let mut h = harness(book.clone(), Recorder::new(kind, CELL), false);
        let update = h.strip.show(StripPosition { page: 4, offset: 0.5 }, request(10, 10));
        assert!(update.shown.is_none(), "the anchor page's size loads on a job");
        assert_position(&h, 4, 0.5);
        h.settle(update);
        assert_position(&h, 4, 0.5);
        let mut frames = h.scroll(30.0);
        frames.extend(h.scroll(-60.0));
        assert!(frames.iter().any(|frame| frame.shown.is_some()));
        let readers = book.readers.lock();
        assert!(!readers.is_empty());
        assert!(!readers.contains(&thread::current().id()), "{kind:?} read the book on the caller's thread");
    }
}

#[test]
fn a_failing_page_decodes_once_for_the_bands_waiting_on_it() {
    // Page 0 fills bands 0 to 2 of 160 pixels; bands 0 and 1 render at once.
    let book = Pages::broken((0..3).map(|i| rows_page(i, 80, 480)).collect(), 0);
    let mut h = harness(book.clone(), Recorder::new(RendererKind::Kitty, CELL), false);
    let shown = h.show(0, 0.0, request(10, 10)).shown.expect("a frame");
    assert_eq!(shown.errors, ["page 1: pdftoppm: broken page"]);
    assert!(matches!(h.view().bands.get(&1), Some(Band::Ready { errors, .. }) if errors.len() == 1));
    assert_eq!(book.decodes.load(Ordering::SeqCst), 1, "the waiting band did not decode again");
    h.scroll(10.0);
    assert_eq!(book.decodes.load(Ordering::SeqCst), 2, "a later band tries again");
}

#[test]
fn band_pixels_join_pages_without_a_gap() {
    let kitty = Recorder::new(RendererKind::Kitty, CELL);
    let mut h = harness(Pages::new((0..5).map(|i| rows_page(i, 80, 100)).collect()), kitty.clone(), false);
    h.show(0, 0.0, request(10, 10));
    // Bands are 160 pixels tall: band 0 holds page 0 and 60 rows of page 1.
    let band0 = kitty.image(h.band(0).id);
    assert_eq!(band0.dimensions(), (80, 160));
    assert_eq!(pixel(&band0, 40, 99), [0, 99, 0, 255]);
    assert_eq!(pixel(&band0, 40, 100), [1, 0, 0, 255]);
    assert_eq!(pixel(&band0, 40, 159), [1, 59, 0, 255]);
    let band1 = kitty.image(h.band(1).id);
    assert_eq!(pixel(&band1, 40, 0), [1, 60, 0, 255]);
    assert_eq!(pixel(&band1, 40, 39), [1, 99, 0, 255]);
    assert_eq!(pixel(&band1, 40, 40), [2, 0, 0, 255]);
}

/// Checks that the kitty rows of `update` fill the viewport of `grid` from
/// resident bands of `grid`, each row inside its band's image.
fn assert_rows_map_into_bands(h: &Harness, update: &Update, grid: Grid) {
    let Some(shown) = &update.shown else { return };
    let mut row = 0;
    for layer in &shown.scene.layers {
        let Layer::KittyRows { id, cols, src_row, rows, at } = *layer else { panic!("not kitty rows: {layer:?}") };
        let band = h.view().bands.values().find_map(|band| match band {
            Band::Ready { lease, frame, .. } if lease.id == id => Some(frame.grid()),
            _ => None,
        });
        assert_eq!(band, Some(grid), "band {id:?} fills its cells");
        assert_eq!((cols, at.row), (grid.cols, row));
        assert!(src_row + rows <= grid.rows, "rows {src_row}+{rows} inside a band of {} rows", grid.rows);
        row += rows;
    }
    assert_eq!(row, grid.rows, "every viewport row shows");
}

#[test]
fn kitty_bands_fill_their_cells_exactly() {
    let sizes = [((8.4, 17.0), (2, 100)), ((8.4, 17.0), (30, 20)), ((7.3, 15.6), (7, 33)), ((9.65, 19.35), (13, 41)), ((6.9, 13.7), (3, 120))];
    for ((cell_w, cell_h), (cols, rows)) in sizes {
        let kitty = Arc::new(Kitty::new(CellSize { w: cell_w, h: cell_h }, false));
        let mut h = harness(Pages::new((0..3).map(|i| rows_page(i, 20, 4000)).collect()), kitty, false);
        let grid = Grid::new(cols, rows);
        let shown = h.show(0, 0.0, request(cols, rows));
        assert!(shown.shown.is_some());
        assert_rows_map_into_bands(&h, &shown, grid);
        h.strip.scroll(f64::from(rows + rows / 2));
        for _ in 0..1000 {
            if !h.strip.moving() {
                break;
            }
            let update = h.strip.tick(Instant::now());
            let update = h.settle(update);
            assert_rows_map_into_bands(&h, &update, grid);
        }
        assert!(!h.strip.moving(), "the scrolling stops");
    }
}

#[test]
fn kitty_bands_tile_the_strip_at_fractional_cell_heights() {
    // Green and blue name each strip row.
    let page = RgbaImage::from_fn(20, 6000, |_, y| Rgba([0, (y >> 8) as u8, y as u8, 255]));
    for ((cell_w, cell_h), (cols, rows)) in [((7.3, 15.6), (7, 33)), ((8.4, 17.3), (9, 41)), ((8.0, 17.3), (5, 7))] {
        let kitty = Recorder::new(RendererKind::Kitty, CellSize { w: cell_w, h: cell_h });
        let mut h = harness(Pages::new(vec![page.clone()]), kitty.clone(), false);
        let grid = Grid::new(cols, rows);
        h.show(0, 0.0, request(cols, rows));
        let mut bands = HashMap::new();
        for _ in 0..4 {
            for (&index, band) in &h.view().bands {
                let Band::Ready { lease, frame, .. } = band else { panic!("band {index} is not ready") };
                assert_eq!(frame.grid(), grid, "band {index} fills its cells");
                bands.insert(index, kitty.image(lease.id));
            }
            h.scroll(f64::from(rows));
        }
        let mut next = 0;
        for index in 0..bands.len() as i64 {
            let band = &bands[&index];
            assert_eq!(band.width(), (f64::from(cols) * cell_w).floor() as u32);
            for y in 0..band.height() {
                let [_, high, low, _] = pixel(band, band.width() / 2, y);
                assert_eq!((u32::from(high) << 8) | u32::from(low), next, "band {index} row {y}");
                next += 1;
            }
        }
        assert!(bands.len() >= 5);
    }
}

#[test]
fn a_page_whose_size_lookup_fails_loads_at_its_own_shape() {
    let pages = vec![rows_page(0, 80, 100), rows_page(1, 80, 300), rows_page(2, 80, 100)];
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::sizeless(pages.clone(), 1, None), sixel.clone(), false);
    h.show(1, 0.0, request(80, 100));
    assert_eq!(pixel(&sixel.last(), 40, 99), [1, 99, 0, 255], "page 1 keeps its rows");
    h.scroll(250.0);
    assert_position(&h, 1, 250.0 / 300.0);
    assert_eq!(pixel(&sixel.last(), 40, 49), [1, (299 % 256) as u8, 0, 255]);
    assert_eq!(pixel(&sixel.last(), 40, 50), [2, 0, 0, 255]);

    // A page that neither sizes nor decodes asks again after a clear.
    let mut h = harness(Pages::sizeless(pages, 1, Some(1)), sixel, false);
    h.show(1, 0.0, request(80, 100));
    assert_eq!(h.strip.cx.sizes.get(&1), Some(&None));
    h.strip.clear();
    assert_eq!(h.strip.cx.sizes.get(&1), None);
}

#[test]
fn a_replaced_frame_job_does_not_show_after_the_current_one() {
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(vec![solid(100, 200, RED), solid(100, 200, BLUE)]), sixel, false);
    h.show(0, 0.0, request(100, 100));
    h.scroll(150.0);
    assert_position(&h, 0, 0.75);
    let next = |h: &mut Harness| h.events.recv_timeout(Duration::from_secs(10)).expect("a job finishes");
    // Job A composes page 0 and its result waits in the queue; job B replaces
    // it after the viewport moved to page 1.
    let view = h.strip.view.as_mut().expect("a shown strip");
    view.compose_frame(&h.strip.cx);
    let stale = next(&mut h);
    let view = h.strip.view.as_mut().expect("a shown strip");
    view.top = 250.0;
    view.compose_frame(&h.strip.cx);
    let current = next(&mut h);

    let update = h.strip.on_event(stale).expect("the strip started the job");
    assert!(update.shown.is_none(), "the replaced job's frame is dropped");
    assert!(h.view().waiting);
    let shown = h.strip.on_event(current).expect("the strip started the job").shown.expect("the current frame");
    assert_eq!(shown.page, 1);
    assert!(!h.view().waiting);
}

#[test]
fn wide_pages_shrink_and_narrow_pages_keep_their_size_centered() {
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(vec![rows_page(0, 200, 100), rows_page(1, 40, 200)]), sixel.clone(), false);
    h.show(0, 0.0, request(100, 100));
    let frame = sixel.last();
    assert_eq!(frame.dimensions(), (100, 100));
    // The wide page shrinks to 100 x 50 and fills the width.
    assert_eq!(pixel(&frame, 0, 25)[3], 255);
    assert_eq!(pixel(&frame, 99, 25)[3], 255);
    // The narrow page keeps 40 columns in the middle; its margins stay clear.
    assert_eq!(pixel(&frame, 30, 60), [1, 10, 0, 255]);
    assert_eq!(pixel(&frame, 69, 60), [1, 10, 0, 255]);
    assert_eq!(pixel(&frame, 29, 60), CLEAR);
    assert_eq!(pixel(&frame, 70, 60), CLEAR);
}

#[test]
fn fill_scales_every_page_to_the_viewport_width() {
    let pages = vec![solid(40, 20, RED), solid(200, 100, BLUE), solid(100, 100, GREEN)];
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(pages.clone()), sixel.clone(), false);
    h.show(0, 0.0, StripRequest { fill: true, ..request(100, 100) });
    // The narrow page grows to 100 x 50, and the wide one shrinks to 100 x 50
    // right under it.
    let frame = sixel.last();
    assert_eq!(pixel(&frame, 0, 0), RED);
    assert_eq!(pixel(&frame, 99, 49), RED);
    assert_eq!(pixel(&frame, 0, 50), BLUE);
    assert_eq!(pixel(&frame, 99, 99), BLUE);
    h.scroll(55.0);
    assert_position(&h, 1, 0.1);

    // Without fill the narrow page keeps its 40 x 20 and the wide one still
    // shrinks.
    let mut h = harness(Pages::new(pages), sixel.clone(), false);
    h.show(0, 0.0, request(100, 100));
    let frame = sixel.last();
    assert_eq!(pixel(&frame, 50, 10), RED);
    assert_eq!(pixel(&frame, 0, 10), CLEAR);
    assert_eq!(pixel(&frame, 0, 20), BLUE);
    assert_eq!(pixel(&frame, 99, 69), BLUE);
    assert_eq!(pixel(&frame, 50, 70), GREEN);
}

#[test]
fn a_page_filled_far_past_the_viewport_scales_only_the_sections_in_view() {
    // 10 x 40,000 pixels at 1,000 wide take 1,000 x 4,000,000.
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(vec![rows_page(0, 10, 40_000)]), sixel.clone(), false);
    let scaled = h.strip.cx.scaled.clone();
    for (offset, scales) in [(0.0, 1), (0.5, 2)] {
        let shown = h.show(0, offset, StripRequest { fill: true, ..request(1000, 100) }).shown.expect("a frame");
        assert!(shown.errors.is_empty(), "{:?}", shown.errors);
        assert_eq!(scaled.scales(), scales, "the viewport lies in one section");
    }
    assert_eq!(scaled.pixels(), 2 * 1000 * u64::from(SECTION_ROWS));
    // Row 2,000,050 of the strip shows row 20,000 of the page.
    assert_eq!(pixel(&sixel.last(), 500, 50), [0, (20_000 % 256) as u8, 0, 255]);
}

#[test]
fn a_section_holds_the_rows_of_the_page_scaled_whole() {
    // Enlarging and shrinking, turned every way and inverted; every page
    // spans a join of sections.
    let cases = [
        ((30, 700), 0, (90, 2100), false),
        ((200, 5000), 0, (50, 1250), false),
        ((700, 30), 1, (90, 2100), false),
        ((5000, 200), 3, (50, 1250), false),
        ((60, 1000), 2, (75, 1250), true),
    ];
    for ((page_w, page_h), quarter_turns, (width, height), invert_page) in cases {
        assert!(height > SECTION_ROWS);
        let page = varied_page(page_w, page_h);
        let book = PageCache::new(Pages::new(vec![page.clone()]));
        let scaled = ScaledPages::default();
        let key = Key { page: 0, quarter_turns, width, height, invert: invert_page };
        let mut whole = scale(&rotate(&page, quarter_turns), width, height);
        if invert_page {
            invert(&mut whole);
        }
        let mut next = 0;
        for index in 0..height.div_ceil(SECTION_ROWS) {
            let part = scaled.get(&book, Section { key, index }, &Cancel::new()).expect("a section");
            assert_eq!(part.top, next);
            for y in 0..part.pixels.height() {
                assert_row_near(&part.pixels, y, &whole, next, &format!("{key:?} section {index} row {y}"));
                next += 1;
            }
        }
        assert_eq!(next, height);
    }

    // A page at its own size comes straight from the page cache.
    let book = Pages::new(vec![varied_page(60, 3000)]);
    let key = Key { page: 0, quarter_turns: 0, width: 60, height: 3000, invert: false };
    let part = ScaledPages::default().get(&PageCache::new(book.clone()), Section { key, index: 2 }, &Cancel::new()).expect("a section");
    assert!(Arc::ptr_eq(&part.pixels, &book.pages[0]) && part.top == 0);
}

#[test]
fn a_frame_across_a_section_join_and_a_page_join_shows_every_row_once() {
    // Filled to 120 wide, page 0 grows to three times its height, past the
    // end of its first section.
    let pages = vec![varied_page(40, SECTION_ROWS / 3 + 30), varied_page(40, 300)];
    let whole: Vec<RgbaImage> = pages.iter().map(|page| scale(page, 120, page.height() * 3)).collect();
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(pages), sixel.clone(), false);
    h.show(0, 0.0, StripRequest { fill: true, ..request(120, 300) });
    let top = SECTION_ROWS - 100;
    h.scroll(f64::from(top));
    let frame = sixel.last();
    assert!(whole[0].height() > SECTION_ROWS && whole[0].height() < top + 300, "the frame spans both joins");
    for y in 0..300 {
        let row = top + y;
        let (page, page_row) = if row < whole[0].height() { (0, row) } else { (1, row - whole[0].height()) };
        assert_row_near(&frame, y, &whole[page], page_row, &format!("frame row {y}"));
    }
}

#[test]
fn scrolling_a_long_strip_keeps_its_sections_within_the_budget() {
    // Filled to 1,000 wide, every page grows to 20 whole sections, and the
    // strip to more pixels than the budget.
    let pages = (0..4).map(|i| rows_page(i, 100, 2 * SECTION_ROWS)).collect();
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(pages), sixel, false);
    h.show(0, 0.0, StripRequest { fill: true, ..request(1000, 100) });
    let scaled = h.strip.cx.scaled.clone();
    assert!(80 * 1000 * u64::from(SECTION_ROWS) > ScaledPages::MAX_PIXELS);
    // A section at a time, each frame inside one.
    for _ in 1..80 {
        h.scroll(f64::from(SECTION_ROWS));
        assert!(scaled.pixels() <= ScaledPages::MAX_PIXELS, "{} pixels", scaled.pixels());
    }
    assert_position(&h, 3, 19.0 / 20.0);
    assert_eq!(scaled.scales(), 80, "every section scaled once");
    assert!(scaled.kept() < 80, "the least recently used sections went");
}

#[test]
fn half_blocks_fill_the_viewport_width() {
    let half = Recorder::new(RendererKind::HalfBlock, CELL);
    let mut h = harness(Pages::new(vec![solid(40, 400, RED)]), half.clone(), false);
    // 25 cells across compose 25 pixels wide; the 40-pixel page fills them
    // rather than shrinking to one pixel per cell.
    h.show(0, 0.0, StripRequest { fill: true, ..request(25, 25) });
    let frame = half.last();
    assert_eq!(frame.dimensions(), (25, 50));
    assert_eq!(pixel(&frame, 0, 10), RED);
    assert_eq!(pixel(&frame, 24, 49), RED);
    assert_eq!(h.view().layout.page_at(0.0).height, 250);
}

#[test]
fn scrolls_across_pages() {
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(vec![solid(100, 200, RED), solid(100, 200, BLUE)]), sixel.clone(), false);
    h.show(0, 0.0, request(100, 100));
    h.scroll(150.0);
    assert_position(&h, 0, 0.75);
    assert_eq!(pixel(&sixel.last(), 50, 25), RED);
    assert_eq!(pixel(&sixel.last(), 50, 75), BLUE);
    h.scroll(100.0);
    assert_position(&h, 1, 0.25);
    assert_eq!(pixel(&sixel.last(), 50, 50), BLUE);
}

#[test]
fn scrolls_backward_across_a_page() {
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(vec![solid(100, 200, RED), solid(100, 200, BLUE)]), sixel.clone(), false);
    h.show(1, 0.25, request(100, 100));
    h.scroll(-100.0);
    assert_position(&h, 0, 0.75);
    assert_eq!(pixel(&sixel.last(), 50, 25), RED);
    assert_eq!(pixel(&sixel.last(), 50, 75), BLUE);
}

#[test]
fn keeps_the_strip_end_at_the_viewport_bottom_and_stops_at_the_top() {
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new(vec![solid(100, 200, RED), solid(100, 50, BLUE)]), sixel.clone(), false);
    h.show(1, 1.0, request(100, 100));
    assert_position(&h, 0, 0.75);
    assert_eq!(pixel(&sixel.last(), 50, 25), RED);
    assert_eq!(pixel(&sixel.last(), 50, 75), BLUE);
    h.scroll(-1000.0);
    assert_position(&h, 0, 0.0);
    assert!(!h.strip.moving(), "the top of page 0 stops the scrolling");
    h.scroll(1000.0);
    assert_position(&h, 0, 0.75);
}

#[test]
fn kitty_stops_at_the_ends_between_rows() {
    // Pages of 100 pixels end between rows of 16.
    let kitty = Recorder::new(RendererKind::Kitty, CELL);
    let mut h = harness(Pages::new((0..3).map(|i| rows_page(i, 80, 100)).collect()), kitty.clone(), false);
    h.show(0, 0.5, request(10, 10));
    h.scroll(100.0);
    assert_position(&h, 1, 0.4);
    let frame = h.scroll(-100.0);
    assert_position(&h, 0, 0.0);
    assert_eq!(kitty_rows(frame.last().unwrap())[0].1, 0, "the bands start again at the top of page 0");
    assert_eq!(pixel(&kitty.image(h.band(0).id), 40, 0), [0, 0, 0, 255]);
}

#[test]
fn inverts_the_pages_the_book_allows() {
    let pages = vec![solid(100, 60, RED), solid(40, 60, BLUE), solid(100, 60, GREEN)];
    let book = Pages::invertible(pages, vec![false, true, false]);
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(book.clone(), sixel.clone(), false);
    h.show(0, 0.0, StripRequest { invert: true, ..request(100, 100) });
    h.scroll(30.0);
    let frame = sixel.last();
    assert_eq!(pixel(&frame, 50, 10), RED);
    assert_eq!(pixel(&frame, 50, 50), [255, 255, 0, 255]);
    assert_eq!(pixel(&frame, 10, 50), CLEAR, "the margins of an inverted page stay clear");
    assert_eq!(pixel(&frame, 50, 95), GREEN);
    assert_eq!(pixel(&book.pages[1], 0, 0), BLUE, "inversion left the decoded page alone");
}

#[test]
fn half_blocks_refuse_text_pages() {
    for (kind, refused) in [(RendererKind::HalfBlock, true), (RendererKind::Sixel, false)] {
        let recorder = Recorder::new(kind, PIXEL);
        let mut book = Arc::into_inner(Pages::new(vec![solid(100, 60, RED), solid(100, 60, BLUE), solid(100, 60, GREEN)])).expect("a new book");
        book.text = Some(1);
        let mut h = harness(Arc::new(book), recorder.clone(), false);
        let shown = h.show(0, 0.0, request(100, 100)).shown.expect("a frame");
        if refused {
            assert_eq!(shown.errors, [TEXT_NEEDS_PIXELS]);
            assert!(shown.scene.layers.is_empty(), "the viewport shows no image");
            assert_eq!(recorder.renders(), 0);
        } else {
            assert!(shown.errors.is_empty() && !shown.scene.layers.is_empty());
            assert_eq!(pixel(&recorder.last(), 50, 90), BLUE);
        }
        // A viewport without the text page shows.
        let shown = h.show(2, 0.5, request(100, 30)).shown.expect("a frame");
        assert!(shown.errors.is_empty() && !shown.scene.layers.is_empty(), "{kind:?}");
    }
    assert!(TEXT_NEEDS_PIXELS.starts_with("text pages in webtoon need pixel rendering: t to leave webtoon, e for browser, "));
}

#[test]
fn half_blocks_compose_at_cell_resolution() {
    let half = Recorder::new(RendererKind::HalfBlock, CELL);
    let mut h = harness(Pages::new(vec![solid(40, 400, RED), solid(400, 400, BLUE)]), half.clone(), false);
    // 25 x 25 cells of 8 x 16 pixels compose at one pixel per cell across:
    // 25 x 50. The narrow page keeps a fifth of the width.
    h.show(0, 0.0, request(25, 25));
    h.scroll(10.0);
    assert_position(&h, 0, 0.4);
    let frame = half.last();
    assert_eq!(frame.dimensions(), (25, 50));
    assert_eq!(pixel(&frame, 12, 10), RED);
    assert_eq!(pixel(&frame, 5, 10), CLEAR);
    assert_eq!(pixel(&frame, 12, 40), BLUE);

    // Cells of 8.4 x 17 pixels make a frame 50.6 pixels tall; rounded up, it
    // would fit its cells by height and leave a column blank.
    let half = Arc::new(HalfBlock::new(CellSize { w: 8.4, h: 17.0 }));
    let mut h = harness(Pages::new(vec![solid(40, 400, RED), solid(400, 400, BLUE)]), half, false);
    let shown = h.show(0, 0.0, request(200, 25)).shown.expect("a frame");
    assert!(matches!(shown.scene.layers[..], [Layer::Cells { grid: Grid { cols: 200, rows: 25 }, .. }]));
}

#[test]
fn show_then_position_returns_that_position() {
    for kind in [RendererKind::Kitty, RendererKind::Sixel, RendererKind::HalfBlock] {
        let mut h = harness(Pages::new((0..6).map(|i| rows_page(i, 80, 400)).collect()), Recorder::new(kind, CELL), false);
        h.show(3, 0.37, request(10, 10));
        assert_position(&h, 3, 0.37);
        h.show(99, 0.5, request(10, 10));
        assert_position(&h, 5, 0.5);
        h.show(2, f64::NAN, request(10, 10));
        assert_position(&h, 2, 0.0);
        h.strip.scroll(f64::INFINITY);
        assert!(!h.strip.moving());
    }
}

/// A native strip of one page 100 pixels wide and `height` tall, in a
/// viewport of 100 x 80 pixels.
fn native(height: u32) -> Harness {
    let mut h = harness(Pages::new(vec![rows_page(0, 100, height)]), Recorder::new(RendererKind::Native, PIXEL), false);
    h.show(0, 0.0, request(100, 80));
    h
}

fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[test]
fn a_glide_eases_out_and_ends_on_its_target() {
    let mut h = native(10_000);
    let start = Instant::now();
    h.strip.scroll(1000.0);
    h.tick(start);
    assert_eq!((h.view().top, h.strip.pending()), (0.0, 1000.0), "the clock starts at the first tick");
    for (elapsed, u) in [(70, 0.25), (140, 0.5)] {
        h.tick(start + ms(elapsed));
        let (top, want) = (h.view().top, 1000.0 * (1.0 - (1.0_f64 - u).powi(3)));
        assert!((top - want).abs() < 1e-6, "after {elapsed} ms: {top}, want {want}");
        assert!((h.strip.pending() - (1000.0 - top)).abs() < 1e-6 && h.strip.moving());
    }
    h.tick(start + Strip::GLIDE);
    assert_eq!((h.view().top, h.strip.pending()), (1000.0, 0.0));
    assert!(!h.strip.moving());
}

#[test]
fn a_move_during_a_glide_goes_on_from_where_the_view_is() {
    let mut h = native(10_000);
    let start = Instant::now();
    h.strip.scroll(1000.0);
    h.tick(start);
    h.tick(start + ms(140));
    let mut tops = vec![h.view().top];
    assert!((tops[0] - 875.0).abs() < 1e-6, "{tops:?}");
    h.strip.scroll(500.0);
    for elapsed in [150, 200, 290, 430] {
        h.tick(start + ms(elapsed));
        tops.push(h.view().top);
    }
    // The new glide starts at the last tick, 10 ms before, with the 625
    // pixels now left to run.
    let want = 875.0 + 625.0 * (1.0 - (1.0 - 10.0 / 280.0_f64).powi(3));
    assert!((tops[1] - want).abs() < 1e-6, "the view moves on through the key press: {tops:?}");
    assert!(tops.windows(2).all(|pair| pair[1] > pair[0]), "no jump back and no pause: {tops:?}");
    assert!((tops[4] - 1500.0).abs() < 1e-9, "the glide ends at the sum of both moves: {tops:?}");
    assert!(!h.strip.moving());

    // A move after the glide ended starts from rest, at its first tick.
    h.strip.scroll(100.0);
    h.tick(start + ms(500));
    assert_eq!(h.view().top, tops[4]);
    h.tick(start + ms(640));
    // Turning around drops what was left of the other direction.
    h.strip.scroll(-1.0);
    assert_eq!(h.strip.pending(), -1.0);
}

#[test]
fn without_a_glide_or_a_renderer_that_slides_a_move_takes_one_tick() {
    let moves = [
        (RendererKind::Native, Duration::ZERO),
        (RendererKind::Kitty, Strip::GLIDE),
        (RendererKind::Sixel, Strip::GLIDE),
        (RendererKind::HalfBlock, Strip::GLIDE),
    ];
    for (kind, glide) in moves {
        let mut h = harness(Pages::new(vec![rows_page(0, 80, 2000)]), Recorder::new(kind, CELL), false);
        assert_eq!(h.strip.slides(), kind == RendererKind::Native);
        h.show(0, 0.0, request(10, 10));
        h.strip.set_glide(glide);
        h.strip.scroll(10.0);
        h.tick(Instant::now());
        assert!(!h.strip.moving(), "{kind:?}");
        assert_eq!(h.view().top, 10.0 * h.view().cell_h, "{kind:?}");
    }
}

#[test]
fn an_end_of_the_strip_stops_a_glide() {
    let mut h = native(1000);
    let start = Instant::now();
    h.strip.scroll(5000.0);
    h.tick(start);
    h.tick(start + ms(140));
    assert_eq!((h.view().top, h.strip.pending()), (920.0, 0.0));
    assert!(!h.strip.moving());
}

#[test]
fn a_shift_moves_at_once_and_ends_a_glide_at_the_sum() {
    let mut h = native(10_000);
    let start = Instant::now();
    h.strip.shift(100.0);
    h.tick(start);
    assert_eq!((h.view().top, h.strip.pending()), (100.0, 0.0));
    h.strip.scroll(1000.0);
    h.tick(start + ms(10));
    assert_eq!(h.view().top, 100.0, "a scroll after a shift glides");
    h.tick(start + ms(150));
    assert!(h.view().top > 100.0 && h.strip.moving());
    h.strip.shift(50.0);
    h.tick(start + ms(160));
    assert!((h.view().top - 1150.0).abs() < 1e-9, "{}", h.view().top);
    assert!(!h.strip.moving());
}

#[test]
fn a_changed_glide_time_and_an_empty_shift_leave_a_glide_moving_on() {
    let mut h = native(10_000);
    let start = Instant::now();
    h.strip.scroll(1000.0);
    h.tick(start);
    h.tick(start + ms(140));
    assert!((h.view().top - 875.0).abs() < 1e-6);
    // The 125 pixels left now take 320 ms from the last tick.
    h.strip.set_glide(ms(320));
    h.tick(start + ms(150));
    let want = 875.0 + 125.0 * (1.0 - (1.0 - 10.0 / 320.0_f64).powi(3));
    assert!((h.view().top - want).abs() < 1e-6, "{}", h.view().top);
    for rows in [f64::NAN, f64::INFINITY] {
        h.strip.shift(rows);
    }
    h.tick(start + ms(160));
    assert!(h.view().top > want && h.view().top < 1000.0 && h.strip.moving(), "{}", h.view().top);
    h.tick(start + ms(460));
    assert_eq!((h.view().top, h.strip.moving()), (1000.0, false));
}

#[test]
fn a_move_longer_than_any_strip_lands_at_its_end() {
    // Five pages of 480 rows, of which a 10 x 10 viewport shows 160.
    let mut h = harness(Pages::new((0..5).map(|i| rows_page(i, 80, 480)).collect()), Recorder::new(RendererKind::Kitty, CELL), false);
    h.show(0, 0.0, request(10, 10));
    for rows in [1e18, f64::MAX] {
        h.strip.scroll(rows);
        // The sizes of the pages on the way arrive after the move.
        h.tick(Instant::now());
        assert_position(&h, 4, 320.0 / 480.0);
        assert!(!h.strip.moving());
    }
    h.strip.scroll(-f64::MAX);
    h.tick(Instant::now());
    assert_position(&h, 0, 0.0);
}

#[test]
fn scrolling_before_the_anchor_page_has_its_size_waits_for_the_view() {
    let sixel = Recorder::new(RendererKind::Sixel, PIXEL);
    let mut h = harness(Pages::new((0..4).map(|i| rows_page(i, 80, 100)).collect()), sixel, false);
    let update = h.strip.show(StripPosition { page: 1, offset: 0.0 }, request(80, 100));
    assert!(update.shown.is_none(), "the anchor page's size loads on a job");
    h.strip.scroll(30.0);
    h.strip.scroll(-10.0);
    assert_eq!(h.strip.pending(), -10.0, "turning around drops the other direction");
    h.strip.scroll(40.0);
    h.strip.scroll(15.0);
    assert_eq!(h.strip.pending(), 55.0);
    h.settle(update);
    assert!(h.strip.moving(), "the view takes the waiting rows");
    h.scroll(0.0);
    assert_position(&h, 1, 0.55);
}

#[test]
fn clear_gives_the_band_ids_back() {
    let mut h = harness(Pages::new((0..4).map(|i| rows_page(i, 80, 400)).collect()), Recorder::new(RendererKind::Kitty, CELL), false);
    h.show(0, 0.0, request(10, 10));
    let leases = [h.band(0), h.band(1)];
    assert!(leases.iter().all(|&lease| h.resident.holds(lease)));
    h.strip.clear();
    assert!(leases.iter().all(|&lease| !h.resident.holds(lease)));
    assert_eq!(h.strip.cx.scaled.kept(), 0);
    assert_eq!(h.strip.position(), StripPosition::default());
}

#[test]
fn under_tmux_rendering_again_draws_under_other_ids_and_then_gives_the_old_ones_back() {
    let mut h = harness(Pages::new((0..4).map(|i| rows_page(i, 80, 400)).collect()), Recorder::new(RendererKind::Kitty, CELL), true);
    let before = h.show(0, 0.3, request(10, 10));
    let leases = [h.band(0), h.band(1)];
    h.strip.clear();
    assert!(leases.iter().all(|&lease| h.resident.holds(lease)), "the ids on screen stay held until a new frame shows");
    let again = h.show(0, 0.3, request(10, 10));
    let drawn = |update: &Update| kitty_rows(update).into_iter().map(|(id, ..)| id).collect::<Vec<_>>();
    assert!(drawn(&again).iter().all(|id| !drawn(&before).contains(id)), "{:?} reuses an id of {:?}", drawn(&again), drawn(&before));
    assert!(leases.iter().all(|&lease| !h.resident.holds(lease)));
}

#[test]
fn kitty_half_viewport_scrolls_add_up_to_a_whole_one() {
    let top_after = |scrolls: &[f64]| {
        let mut h = harness(Pages::new((0..4).map(|i| rows_page(i, 80, 400)).collect()), Recorder::new(RendererKind::Kitty, CELL), false);
        h.show(0, 0.0, request(10, 9));
        for &rows in scrolls {
            h.scroll(rows);
        }
        h.view().top / CELL.h
    };
    assert_eq!(top_after(&[4.5, 4.5]), top_after(&[9.0]));
    assert_eq!(top_after(&[4.5, 4.5, -9.0]), 0.0);
    assert_eq!(top_after(&[4.5]).fract(), 0.0, "placeholder cells move in whole rows");
}

#[test]
fn kitty_drops_the_half_row_on_landing_at_an_end() {
    // Two pages of 25 rows under a 9-row viewport: the bottom is row 41.
    let top_after = |scrolls: &[f64]| {
        let mut h = harness(Pages::new((0..2).map(|i| rows_page(i, 80, 400)).collect()), Recorder::new(RendererKind::Kitty, CELL), false);
        h.show(0, 0.0, request(10, 9));
        for &rows in scrolls {
            h.scroll(rows);
        }
        h.view().top / CELL.h
    };
    assert_eq!(top_after(&[36.0, 4.5]), 41.0, "half a viewport lands on the bottom");
    assert_eq!(top_after(&[36.0, 4.5, -9.0]), 32.0);
    assert_eq!(top_after(&[5.0, -4.5]), 0.0, "half a viewport lands on the top");
    assert_eq!(top_after(&[5.0, -4.5, 9.0]), 9.0);
}

#[test]
fn a_point_of_the_viewport_finds_its_page() {
    let book = Pages::new((0..3).map(|i| rows_page(i, 100, 1000)).collect());
    let mut h = harness(book, Recorder::new(RendererKind::Native, PIXEL), false);
    h.show(1, 0.5, request(200, 80));
    // Narrow pages sit centered, 50 pixels from either side.
    let (page, x, y) = h.strip.page_at(60.0, 40.0).expect("a page under the point");
    assert!(page == 1 && (x - 0.1).abs() < 1e-9 && (y - 0.54).abs() < 1e-9, "{page} {x} {y}");
    assert_eq!(h.strip.page_at(10.0, 40.0), None, "the margin");
    h.show(1, 0.95, request(200, 80));
    let (page, _, y) = h.strip.page_at(60.0, 70.0).expect("the next page");
    assert!(page == 2 && (y - 0.02).abs() < 1e-9, "{page} {y}");
}
