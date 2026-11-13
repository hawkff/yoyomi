use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};
use parking_lot::Mutex;
use yoyomi_core::image::Rgba;
use yoyomi_core::{CellSize, Error, ImageIds, PageKind, Pixels, RgbaImage, fit_box};
use yoyomi_render::{HalfBlock, Kitty};

use super::scaled::TEXT_NEEDS_PIXELS;
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
    StripRequest { grid: Grid::new(cols, rows), quarter_turns: 0, invert: false }
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

    /// Scrolls `rows` and eases until the scrolling stops, settling every
    /// frame. Returns the update of every frame.
    fn scroll(&mut self, rows: f64) -> Vec<Update> {
        self.strip.scroll(rows);
        let mut frames = Vec::new();
        while self.strip.moving() {
            let update = self.strip.tick();
            frames.push(self.settle(update));
            assert!(frames.len() < 1000, "the easing stops");
        }
        frames
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
    for _ in 0..2 {
        h.scroll(60.0);
        h.scroll(-60.0);
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
            let update = h.strip.tick();
            let update = h.settle(update);
            assert_rows_map_into_bands(&h, &update, grid);
        }
        assert!(!h.strip.moving(), "the easing stops");
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

#[test]
fn easing_reaches_the_pending_amount_and_stops() {
    for (kind, cell) in [(RendererKind::Sixel, PIXEL), (RendererKind::Kitty, CELL)] {
        let mut h = harness(Pages::new(vec![rows_page(0, 80, 2000)]), Recorder::new(kind, cell), false);
        h.show(0, 0.0, request(10, 10));
        h.strip.scroll(10.0);
        let mut tops = vec![0.0];
        while h.strip.moving() {
            let update = h.strip.tick();
            h.settle(update);
            tops.push(h.view().top);
            assert!(tops.len() < 40, "the easing stops");
        }
        let rows: Vec<f64> = tops.windows(2).map(|step| (step[1] - step[0]) / cell.h).collect();
        assert!(rows.iter().all(|&step| step > 0.0 && step <= 3.0 + 1e-9), "steps of {rows:?}");
        if kind == RendererKind::Kitty {
            assert!(rows.iter().all(|step| step.fract() == 0.0), "kitty moves in whole rows: {rows:?}");
        }
        assert!((tops.last().unwrap() - 10.0 * cell.h).abs() < 1e-9);

        // Turning around drops what was left of the other direction.
        h.strip.scroll(10.0);
        h.strip.tick();
        h.strip.scroll(-1.0);
        assert_eq!(h.view().pending, -1.0);
    }

    // Kitty rounds a half row of scrolling to whole rows and stops.
    let mut h = harness(Pages::new(vec![rows_page(0, 80, 2000)]), Recorder::new(RendererKind::Kitty, CELL), false);
    h.show(0, 0.0, request(10, 10));
    h.scroll(49.5);
    assert!(!h.strip.moving());
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
fn native_scrolling_eases_with_a_velocity_of_at_most_64_pixels() {
    let mut h = harness(Pages::new(vec![rows_page(0, 100, 10_000)]), Recorder::new(RendererKind::Native, PIXEL), false);
    h.show(0, 0.0, request(100, 80));
    h.strip.scroll(100.0);
    let update = h.strip.tick();
    h.settle(update);
    // The first frame moves (0 + 100 * 0.075) * 0.8 pixels.
    assert_position(&h, 0, 6.0 / 10_000.0);
    h.scroll(0.0);
    let top = h.view().top;
    assert!((top - 100.0).abs() < 0.05, "the easing ends at its target: {top}");

    h.strip.scroll(2000.0);
    let mut steps = Vec::new();
    while h.strip.moving() {
        let before = h.view().top;
        let update = h.strip.tick();
        h.settle(update);
        steps.push(h.view().top - before);
    }
    assert!(steps.iter().all(|step| *step <= 64.0 + 1e-9) && steps.iter().any(|step| (step - 64.0).abs() < 1e-6), "{steps:?}");

    // Turning around drops the velocity with the distance left.
    h.strip.scroll(100.0);
    h.strip.tick();
    h.strip.scroll(-1.0);
    assert_eq!((h.view().pending, h.view().velocity), (-1.0, 0.0));
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
