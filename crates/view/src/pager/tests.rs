use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use yoyomi_core::{CellSize, Error, ImageIds, PageKind, PageRect, Pixels, Rgb, RgbaImage};

use super::*;

/// A book of `size` pages whose red channel holds the page index. It records
/// its decodes, fails on `failing`, and holds the first decode of `held` until
/// its request is cancelled or `released` is set.
struct FakeBook {
    path: PathBuf,
    len: usize,
    size: (u32, u32),
    failing: Option<usize>,
    held: Mutex<Option<usize>>,
    holding: AtomicBool,
    released: AtomicBool,
    decodes: Mutex<Vec<usize>>,
    cancelled: AtomicUsize,
}

impl Book for FakeBook {
    fn path(&self) -> &Path {
        &self.path
    }

    fn title(&self) -> &str {
        "fake"
    }

    fn len(&self) -> usize {
        self.len
    }

    fn kind(&self, _page: usize) -> PageKind {
        PageKind::Image
    }

    /// Odd pages keep their colors, as illustrations in a text book do.
    fn can_invert(&self, page: usize) -> bool {
        page.is_multiple_of(2)
    }

    fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
        self.decodes.lock().push(page);
        if self.held.lock().take_if(|held| *held == page).is_some() {
            self.holding.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !cancel.is_cancelled() && !self.released.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if cancel.is_cancelled() {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                return Err(Error::Cancelled);
            }
        }
        if self.failing == Some(page) {
            return Err(Error::Decode("broken page".into()));
        }
        Ok(Arc::new(RgbaImage::from_pixel(self.size.0, self.size.1, [page as u8, 0, 0, 255].into())))
    }
}

/// A renderer of one kind that records each page it renders, with its invert
/// flag. Frames are at most 6 x 8 cells, and kitty uploads are 100 bytes.
struct Recording {
    kind: RendererKind,
    cell: Mutex<CellSize>,
    renders: Mutex<Vec<(usize, bool)>>,
}

impl Renderer for Recording {
    fn kind(&self) -> RendererKind {
        self.kind
    }

    fn cell_size(&self) -> CellSize {
        *self.cell.lock()
    }

    fn set_cell_size(&self, cell: CellSize) {
        *self.cell.lock() = cell;
    }

    fn render(&self, image: &RgbaImage, id: ImageId, max: Grid, invert: bool) -> Result<Frame> {
        self.renders.lock().push((usize::from(image.get_pixel(0, 0)[0]), invert));
        let grid = Grid::new(max.cols.min(6), max.rows.min(8));
        Ok(match self.kind {
            RendererKind::Kitty => Frame::Kitty { id, grid, upload: vec![7; UPLOAD].into() },
            RendererKind::Sixel => Frame::Sixel { grid, data: vec![8; 10].into() },
            _ => Frame::Cells { grid, colors: vec![[Rgb(1, 2, 3); 2]; usize::from(grid.cols * grid.rows)].into() },
        })
    }

    fn delete(&self, _id: ImageId) -> Vec<u8> {
        Vec::new()
    }
}

const UPLOAD: usize = 100;
const KINDS: [RendererKind; 3] = [RendererKind::Kitty, RendererKind::Sixel, RendererKind::HalfBlock];

struct Rig {
    pager: Pager,
    book: Arc<FakeBook>,
    renderer: Arc<Recording>,
    resident: Arc<Resident>,
    events: Receiver<ViewEvent>,
}

fn rig(kind: RendererKind, len: usize, tmux: bool) -> Rig {
    rig_with(kind, len, tmux, None, None, (1, 1))
}

fn rig_with(kind: RendererKind, len: usize, tmux: bool, failing: Option<usize>, held: Option<usize>, size: (u32, u32)) -> Rig {
    let book = Arc::new(FakeBook {
        path: PathBuf::from("/books/fake.cbz"),
        len,
        size,
        failing,
        held: Mutex::new(held),
        holding: AtomicBool::new(false),
        released: AtomicBool::new(false),
        decodes: Mutex::new(Vec::new()),
        cancelled: AtomicUsize::new(0),
    });
    let renderer = Arc::new(Recording { kind, cell: Mutex::new(CellSize::FALLBACK), renders: Mutex::new(Vec::new()) });
    let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), tmux));
    let (sender, events) = crossbeam_channel::unbounded();
    let pager = Pager::new(Arc::new(PageCache::new(book.clone())), renderer.clone(), Jobs::new(2), resident.clone(), sender);
    Rig { pager, book, renderer, resident, events }
}

impl Rig {
    /// Hands the pager its events until no job of it runs.
    fn settle(&mut self) -> Vec<Update> {
        let mut updates = Vec::new();
        while !self.pager.running.is_empty() {
            let event = self.events.recv_timeout(Duration::from_secs(5)).expect("a job finished");
            updates.push(self.pager.on_event(event).expect("the pager's own event"));
        }
        updates
    }

    fn renders(&self) -> usize {
        self.renderer.renders.lock().len()
    }

    /// Renders of `pages`, which neighbors rendering in the background
    /// cannot change.
    fn renders_of(&self, pages: &[usize]) -> usize {
        self.renderer.renders.lock().iter().filter(|(page, _)| pages.contains(page)).count()
    }

    fn decodes_of(&self, pages: &[usize]) -> usize {
        self.book.decodes.lock().iter().filter(|page| pages.contains(page)).count()
    }

    /// Waits until the decode of the held page started.
    fn wait_holding(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.book.holding.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the held page started decoding");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The job rendering `page`.
    fn running(&self, page: usize) -> &Running {
        self.pager.running.iter().find(|(key, _)| key.page == page).map(|(_, running)| running).expect("a job renders the page")
    }
}

/// The kitty id a finished job holds.
fn lease_of(event: &ViewEvent) -> Option<Lease> {
    event.payload.downcast_ref::<Rendered>().and_then(|rendered| rendered.claim.lease)
}

fn request(page: usize, spread: bool) -> PageRequest {
    PageRequest { page, spread, grid: Grid::new(21, 10), margin: Grid::default(), transform: Transform::default(), selected_page: page, text_top: 0 }
}

fn upload_bytes(updates: &[Update]) -> usize {
    updates.iter().flat_map(|update| &update.uploads).map(|upload| upload.bytes.len()).sum()
}

fn shown(updates: Vec<Update>) -> Shown {
    let mut shown: Vec<Shown> = updates.into_iter().filter_map(|update| update.shown).collect();
    assert_eq!(shown.len(), 1, "one frame answers the request");
    shown.remove(0)
}

fn columns(shown: &Shown) -> Vec<At> {
    shown
        .scene
        .layers
        .iter()
        .map(|layer| match layer {
            Layer::KittyRows { at, .. } | Layer::Sixel { at, .. } | Layer::Cells { at, .. } => *at,
            _ => panic!("the pager draws no text"),
        })
        .collect()
}

#[test]
fn a_turn_to_the_readied_page_renders_and_uploads_nothing() {
    for kind in KINDS {
        let mut rig = rig(kind, 5, false);
        assert!(rig.pager.show(request(0, false)).shown.is_none(), "page 0 renders first");
        let updates = rig.settle();
        assert_eq!(shown(updates.clone()).page, 0);
        assert_eq!(rig.renders(), 2, "page 0, then page 1 in the background");
        if kind == RendererKind::Kitty {
            assert_eq!(upload_bytes(&updates), 2 * UPLOAD);
            let ahead = updates.iter().find(|update| update.shown.is_none()).expect("the upload of page 1");
            assert_eq!(ahead.uploads.len(), 1, "page 1 uploads as soon as it renders");
        }

        let turn = rig.pager.show(request(1, false));
        let shown = turn.shown.expect("page 1 is ready");
        assert_eq!((shown.page, rig.renders_of(&[0, 1]), rig.decodes_of(&[0, 1]), turn.uploads.len()), (1, 2, 2, 0), "{kind:?}");
        assert_eq!(columns(&shown), [At { col: 7, row: 1 }]);
        let layer = &shown.scene.layers[0];
        match kind {
            RendererKind::Kitty => assert!(matches!(layer, Layer::KittyRows { cols: 6, rows: 8, .. })),
            RendererKind::Sixel => assert!(matches!(layer, Layer::Sixel { .. })),
            _ => assert!(matches!(layer, Layer::Cells { .. })),
        }

        rig.settle();
        assert_eq!(rig.renders(), 3, "page 2 readies, page 0 is still there");
        let back = rig.pager.show(request(0, false));
        assert!(back.shown.is_some() && back.uploads.is_empty());
        assert_eq!(rig.renders(), 3, "a turn back renders nothing");
    }
}

#[test]
fn spreads_ready_the_next_pair() {
    let mut rig = rig(RendererKind::Kitty, 6, false);
    rig.pager.show(request(0, true));
    let first = shown(rig.settle());
    // The left slot is 10 cells wide, and the right one starts after the gap.
    assert_eq!(columns(&first), [At { col: 2, row: 1 }, At { col: 13, row: 1 }]);
    assert_eq!(rig.renders(), 4, "pages 0 and 1, then 2 and 3");

    let turn = rig.pager.show(request(2, true));
    assert_eq!(turn.shown.as_ref().expect("pages 2 and 3 are ready").scene.layers.len(), 2);
    assert_eq!((rig.renders_of(&[0, 1, 2, 3]), upload_bytes(&[turn])), (4, 0));

    rig.settle();
    let back = rig.pager.show(request(0, true));
    assert!(back.shown.is_some() && back.uploads.is_empty());
    assert_eq!(rig.renders(), 6, "only pages 4 and 5 rendered since");
}

#[test]
fn pixel_spreads_keep_a_gap_of_a_300th_of_the_width() {
    let mut rig = rig(RendererKind::Native, 4, false);
    rig.pager.show(PageRequest { grid: Grid::new(900, 10), ..request(0, true) });
    // A gap of 3 pixels leaves 448 for the left slot and 449 for the right one.
    assert_eq!(columns(&shown(rig.settle())), [At { col: 221, row: 1 }, At { col: 672, row: 1 }]);
}

#[test]
fn a_native_pager_keeps_four_frames_with_the_next_page_ready() {
    let mut rig = rig(RendererKind::Native, 12, false);
    let turns = [(0, false), (1, false), (2, false), (3, true), (5, true), (7, true), (5, true), (4, false), (9, false), (8, true)];
    for (page, spread) in turns {
        rig.pager.show(request(page, spread));
        while !rig.pager.running.is_empty() {
            let event = rig.events.recv_timeout(Duration::from_secs(5)).expect("a job finished");
            rig.pager.on_event(event).expect("the pager's own event");
            assert!(rig.pager.frames.len() <= 4, "{} frames at page {page}", rig.pager.frames.len());
        }
        let request = request(page, spread);
        let next = rig.pager.neighbors(&request).remove(0);
        assert!(next.iter().all(|slot| rig.pager.find(&slot.key).is_some()), "the page after {page} is ready");
        assert!(rig.pager.slots(&request).iter().all(|slot| rig.pager.find(&slot.key).is_some()), "page {page} stays");
    }
}

#[test]
fn each_page_of_a_spread_takes_its_own_highlights() {
    let rig = rig(RendererKind::Kitty, 5, false);
    let highlights: Arc<[PageRect]> = Arc::from([PageRect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 }]);
    for selected_page in [0, 1] {
        let request =
            PageRequest { transform: Transform { highlights: highlights.clone(), ..Transform::default() }, selected_page, ..request(0, true) };
        let lit: Vec<bool> = rig.pager.slots(&request).iter().map(|slot| !slot.key.transform.highlights.is_empty()).collect();
        assert_eq!(lit, [selected_page == 0, selected_page == 1], "selected page {selected_page}");
    }
}

#[test]
fn neighbors_render_without_the_shown_pages_highlights() {
    let mut rig = rig(RendererKind::Kitty, 5, false);
    let highlights: Arc<[PageRect]> = Arc::from([PageRect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 }]);
    rig.pager.show(PageRequest { transform: Transform { highlights, ..Transform::default() }, ..request(1, false) });
    rig.settle();
    let turn = rig.pager.show(request(2, false));
    assert!(turn.shown.is_some(), "page 2 is ready");
    assert!(rig.pager.running.keys().all(|key| key.page != 2), "page 2 starts no render");
    assert_eq!(rig.renders_of(&[2]), 1);
}

/// The fit box and the place of a frame of `frame` in each slot of a
/// `cols` x `rows` body with the margins outside focus mode: two columns and
/// one row.
fn placed(spread: bool, (cols, rows): (u16, u16), frame: Grid) -> Vec<(Grid, At)> {
    let rig = rig(RendererKind::Sixel, 5, false);
    let request = PageRequest { grid: Grid::new(cols, rows), margin: Grid::new(2, 1), ..request(0, spread) };
    rig.pager.slots(&request).iter().map(|slot| (slot.key.max, slot.at(frame))).collect()
}

#[test]
fn pages_fit_and_center_in_their_slots() {
    // 80 x 24: the body is 80 x 22 below the title. A spread halves 80 to 39,
    // less 2 for the margins, and centers each page in its half of the body:
    // the left from column 0, 39 wide; the right from 40, 40 wide.
    let page = Grid::new(37, 15);
    assert_eq!(placed(true, (80, 22), page), [(Grid::new(37, 21), At { col: 1, row: 3 }), (Grid::new(37, 21), At { col: 41, row: 3 })]);
    // 81 wide: halves of 40 and 40 from column 41, fits of 38.
    let page = Grid::new(38, 16);
    assert_eq!(placed(true, (81, 22), page), [(Grid::new(38, 21), At { col: 1, row: 3 }), (Grid::new(38, 21), At { col: 42, row: 3 })]);
    // One page fits 78 x 21 and centers in the 80 x 22 body: an even
    // height leaves (22 - 20) / 2 rows above it, an odd one (22 - 21) / 2.
    assert_eq!(placed(false, (80, 22), Grid::new(30, 20)), [(Grid::new(78, 21), At { col: 25, row: 1 })]);
    assert_eq!(placed(false, (80, 22), Grid::new(31, 21)), [(Grid::new(78, 21), At { col: 24, row: 0 })]);
    // 81 x 25: a body of 81 x 23, fit 79 x 22.
    assert_eq!(placed(false, (81, 23), Grid::new(30, 20)), [(Grid::new(79, 22), At { col: 25, row: 1 })]);
    assert_eq!(placed(false, (81, 23), Grid::new(31, 21)), [(Grid::new(79, 22), At { col: 25, row: 1 })]);
}

#[test]
fn the_last_page_of_a_spread_shows_alone_on_the_left() {
    let mut rig = rig(RendererKind::Sixel, 3, false);
    rig.pager.show(request(2, true));
    assert_eq!(columns(&shown(rig.settle())), [At { col: 2, row: 1 }]);
}

#[test]
fn spread_turns_stop_on_the_last_pair() {
    // Three pages pair as 1-2 and 3; from 2-3 the next spread is already on screen.
    for (from, delta, want) in [(0, 1, 2), (1, 1, 1), (2, 1, 2), (0, 5, 2), (2, -1, 0)] {
        assert_eq!(turn(from, delta, true, 3), want, "turn {delta} from page {from}");
    }
    assert_eq!(turn(2, 1, false, 3), 2);
    assert_eq!(turn(0, -1, false, 3), 0);
}

#[test]
fn a_newer_request_cancels_the_older_one() {
    let mut rig = rig_with(RendererKind::Kitty, 5, false, None, Some(0), (1, 1));
    rig.pager.show(request(0, false));
    rig.wait_holding();
    rig.pager.show(request(1, false));
    assert_eq!(shown(rig.settle()).page, 1);
    assert_eq!(rig.book.cancelled.load(Ordering::SeqCst), 1, "the decode of page 0 stopped");
    let renders = rig.renderer.renders.lock().clone();
    assert_eq!(renders[0].0, 1, "the old request never reached the renderer");
    assert_eq!(renders.iter().filter(|(page, _)| *page == 0).count(), 1, "page 0 rendered once, as a neighbor");
}

#[test]
fn under_tmux_a_page_uploads_with_every_frame_that_shows_it() {
    let mut rig = rig(RendererKind::Kitty, 5, true);
    rig.pager.show(request(0, false));
    let updates = rig.settle();
    assert_eq!(rig.renders_of(&[1]), 1, "page 1 readies in the background");
    let shown = updates.iter().find(|update| update.shown.is_some()).expect("page 0 shows");
    let page0 = shown.uploads.clone();
    assert_eq!(page0.len(), 1);
    assert_eq!(upload_bytes(&updates), UPLOAD, "page 1 sends nothing ahead");
    let first = rig.pager.show(request(1, false));
    let Some(Layer::KittyRows { id, .. }) = first.shown.expect("page 1 is ready").scene.layers.first().cloned() else { panic!("a kitty frame") };
    assert_eq!(first.uploads.iter().map(|upload| (upload.id, upload.bytes.len())).collect::<Vec<_>>(), [(id, UPLOAD)]);
    assert_eq!(rig.renders_of(&[0, 1]), 2);
    let again = rig.pager.show(request(1, false));
    assert!(again.shown.is_some() && Arc::ptr_eq(&again.uploads[0].bytes, &first.uploads[0].bytes));
    rig.settle();
    let back = rig.pager.show(request(0, false));
    assert!(back.shown.is_some(), "page 0 is still cached");
    assert_eq!(rig.renders_of(&[0]), 1, "the return renders nothing");
    assert!(Arc::ptr_eq(&back.uploads[0].bytes, &page0[0].bytes), "the cached frame carries its upload");
}

#[test]
fn a_frame_whose_id_went_elsewhere_renders_again() {
    let mut rig = rig(RendererKind::Kitty, 5, false);
    rig.pager.show(request(0, false));
    rig.settle();
    let i = rig.pager.frames.iter().position(|cached| cached.key.page == 1).expect("page 1 is ready");
    let lease = rig.pager.frames[i].lease.expect("kitty frames hold a lease");
    rig.resident.release(lease);
    let other = rig.resident.lease();
    assert!(rig.pager.show(request(1, false)).shown.is_none(), "page 1 renders again");
    let updates = rig.settle();
    assert_eq!(shown(updates.clone()).page, 1);
    assert_eq!(rig.renders(), 4, "page 1 again, then page 2");
    assert_eq!(upload_bytes(&updates), 2 * UPLOAD);
    assert!(rig.resident.holds(other));
}

#[test]
fn a_failing_page_reports_its_error() {
    let mut rig = rig_with(RendererKind::Kitty, 6, false, Some(3), None, (1, 1));
    rig.pager.show(request(2, true));
    let spread = shown(rig.settle());
    assert_eq!(spread.scene.layers.len(), 1, "the failed slot stays empty");
    assert_eq!(spread.errors, ["page 4: broken page"]);
    rig.pager.show(request(3, false));
    let single = shown(rig.settle());
    assert!(single.scene.layers.is_empty() && single.errors.len() == 1);
}

#[test]
fn a_new_grid_cell_size_or_transform_renders_again() {
    let mut rig = rig(RendererKind::HalfBlock, 5, false);
    rig.pager.show(request(0, false));
    rig.settle();
    let inverted = Transform { invert: true, ..Transform::default() };
    let odd = rig.pager.show(PageRequest { transform: inverted.clone(), ..request(1, false) });
    assert!(odd.shown.is_some(), "page 1 cannot invert, so its frame stays");

    let zoomed = Transform { zoom: 2.0, ..Transform::default() };
    let changes = [
        PageRequest { transform: inverted, ..request(0, false) },
        PageRequest { transform: zoomed, ..request(0, false) },
        PageRequest { grid: Grid::new(30, 12), ..request(0, false) },
        PageRequest { margin: Grid::new(2, 1), ..request(0, false) },
    ];
    for change in changes {
        assert!(rig.pager.show(change.clone()).shown.is_none(), "{change:?} renders a new frame");
        assert_eq!(shown(rig.settle()).page, 0);
    }
    assert!(rig.renderer.renders.lock().contains(&(0, true)), "page 0 rendered inverted");

    rig.renderer.set_cell_size(CellSize { w: 10.0, h: 20.0 });
    assert!(rig.pager.show(request(0, false)).shown.is_none(), "a new cell size renders a new frame");
}

#[test]
fn clear_releases_every_id() {
    let mut rig = rig(RendererKind::Kitty, 5, false);
    rig.pager.show(request(0, false));
    rig.settle();
    let leases: Vec<Lease> = rig.pager.frames.iter().filter_map(|cached| cached.lease).collect();
    assert_eq!(leases.len(), 2);
    rig.pager.clear();
    assert!(leases.iter().all(|&lease| !rig.resident.holds(lease)));
    assert!(rig.pager.show(request(0, false)).shown.is_none(), "page 0 renders again");
}

#[test]
fn a_frame_whose_id_went_elsewhere_while_rendering_renders_again() {
    let mut rig = rig(RendererKind::Kitty, 5, false);
    rig.pager.show(request(0, false));
    let late = rig.events.recv_timeout(Duration::from_secs(5)).expect("page 0 rendered");
    let lost = lease_of(&late).expect("kitty jobs hold a lease");
    // With every id taken, the next lease takes the least recently shown one.
    let mut thief = rig.resident.lease();
    for _ in 1..ImageIds::PER_PROCESS {
        thief = rig.resident.lease();
    }
    assert_eq!(thief.id, lost.id);
    let update = rig.pager.on_event(late).expect("the pager's own event");
    assert!(update.shown.is_none() && update.uploads.is_empty(), "the late frame neither shows nor uploads");
    let updates = rig.settle();
    let Some(Layer::KittyRows { id, .. }) = shown(updates.clone()).scene.layers.first().cloned() else { panic!("a kitty frame") };
    assert_ne!(id, thief.id);
    assert!(updates.iter().flat_map(|update| &update.uploads).all(|upload| upload.id != thief.id));
    assert_eq!(rig.renders_of(&[0]), 2, "page 0 rendered again");
    assert!(rig.resident.holds(thief));
}

#[test]
fn neighbors_stop_before_they_push_the_shown_pages_out() {
    // Two 11-megapixel pages on screen leave room for the next two, not for four.
    let mut rig = rig_with(RendererKind::HalfBlock, 6, false, None, None, (4000, 2750));
    rig.pager.show(request(2, true));
    assert_eq!(shown(rig.settle()).page, 2);
    assert_eq!(rig.renders_of(&[4, 5]), 2, "the next spread readies");
    assert_eq!(rig.decodes_of(&[0, 1]), 0, "the previous spread waits");
    assert!(rig.pager.book.cached(2).is_some() && rig.pager.book.cached(3).is_some(), "the shown pages stay decoded");
}

#[test]
fn a_queued_neighbor_that_shows_gives_way_to_a_visible_job() {
    let mut rig = rig(RendererKind::Sixel, 5, false);
    rig.pager.show(request(0, false));
    rig.settle();
    // Hold both workers, so the neighbor of page 1 stays queued.
    let (gate, wait) = crossbeam_channel::unbounded::<()>();
    for _ in 0..2 {
        let wait = wait.clone();
        rig.pager.jobs.spawn(Priority::Visible, &Cancel::new(), move || _ = wait.recv());
    }
    assert!(rig.pager.show(request(1, false)).shown.is_some());
    let queued = rig.running(2).job;
    assert_eq!(rig.running(2).priority, Priority::Neighbor);
    assert!(rig.pager.show(request(2, false)).shown.is_none());
    assert_eq!(rig.running(2).priority, Priority::Visible);
    assert_ne!(rig.running(2).job, queued);
    drop(gate);
    assert_eq!(shown(rig.settle()).page, 2);
    assert_eq!(rig.renders_of(&[2]), 1, "the queued neighbor never ran");
}

#[test]
fn a_started_neighbor_that_shows_keeps_rendering() {
    let mut rig = rig_with(RendererKind::Sixel, 5, false, None, Some(2), (1, 1));
    rig.pager.show(request(0, false));
    rig.settle();
    rig.pager.show(request(1, false));
    rig.wait_holding();
    let started = rig.running(2).job;
    assert!(rig.pager.show(request(2, false)).shown.is_none());
    assert_eq!((rig.running(2).job, rig.running(2).priority), (started, Priority::Neighbor));
    rig.book.released.store(true, Ordering::SeqCst);
    assert_eq!(shown(rig.settle()).page, 2);
    assert_eq!((rig.decodes_of(&[2]), rig.book.cancelled.load(Ordering::SeqCst)), (1, 0));
}

#[test]
fn dropping_a_pager_gives_every_id_back() {
    let mut rig = rig(RendererKind::Kitty, 5, false);
    rig.pager.show(request(0, false));
    rig.settle();
    let mut leases: Vec<Lease> = rig.pager.frames.iter().filter_map(|cached| cached.lease).collect();
    rig.pager.show(request(2, false));
    let late = rig.events.recv_timeout(Duration::from_secs(5)).expect("page 2 rendered");
    leases.extend(lease_of(&late));
    assert_eq!(leases.len(), 3);
    drop(rig.pager);
    assert!(leases[..2].iter().all(|&lease| !rig.resident.holds(lease)), "the frames gave their ids back");
    // The app drops an event no pager takes.
    drop(late);
    assert!(!rig.resident.holds(leases[2]), "the late result gave its id back");
}

/// A text book of three pages that each hold `paragraphs`.
struct TextFake {
    path: PathBuf,
    paragraphs: Vec<String>,
}

impl Book for TextFake {
    fn path(&self) -> &Path {
        &self.path
    }

    fn title(&self) -> &str {
        "text"
    }

    fn len(&self) -> usize {
        3
    }

    fn kind(&self, _page: usize) -> PageKind {
        PageKind::Text
    }

    fn page(&self, _page: usize, _cancel: &Cancel) -> Result<Pixels> {
        Ok(Arc::new(RgbaImage::new(1, 1)))
    }

    fn page_text(&self, _page: usize) -> Option<Vec<String>> {
        Some(self.paragraphs.clone())
    }
}

#[test]
fn text_pages_show_as_plain_text_under_half_blocks() {
    let book = Arc::new(TextFake { path: PathBuf::from("/books/text.epub"), paragraphs: vec!["one two three four five".into(), "x".repeat(30)] });
    let renderer = Arc::new(Recording { kind: RendererKind::HalfBlock, cell: Mutex::new(CellSize::FALLBACK), renders: Mutex::new(Vec::new()) });
    let (sender, _events) = crossbeam_channel::unbounded();
    let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), false));
    let mut pager = Pager::new(Arc::new(PageCache::new(book)), renderer.clone(), Jobs::new(2), resident, sender);
    let lines = |update: Update| match &update.shown.expect("plain text shows at once").scene.layers[..] {
        [Layer::Text { lines, at }] => (lines.to_vec(), *at),
        other => panic!("{other:?}"),
    };
    let request = PageRequest { grid: Grid::new(10, 3), ..request(0, false) };
    assert_eq!(pager.text_rows(&request), 6);
    let (top, at) = lines(pager.show(request.clone()));
    assert_eq!(top, ["one two   ", "three four", "\u{2193} 4 more \u{b7}"]);
    assert_eq!(at, At::default());
    let (scrolled, _) = lines(pager.show(PageRequest { text_top: 2, ..request.clone() }));
    assert_eq!(scrolled, ["five      ", "xxxxxxxxxx", "\u{2193} 2 more \u{b7}"]);
    let (end, _) = lines(pager.show(PageRequest { text_top: 99, ..request }));
    assert_eq!(end, ["xxxxxxxxxx"; 3], "scrolling stops at the last row");
    assert!(renderer.renders.lock().is_empty() && pager.running.is_empty(), "text pages render no pixels");
}

#[test]
fn plain_text_wraps_pads_and_cuts_by_terminal_cells() {
    // Wide characters take two cells: a word of them breaks between them.
    assert_eq!(wrap(&["日本語のテキスト".into()], 10), ["日本語のテ", "キスト"]);
    assert_eq!(wrap(&["ab 日本語".into()], 5), ["ab", "日本", "語"]);
    // A character wider than the line still takes one.
    assert_eq!(wrap(&["日本".into()], 1), ["日", "本"]);
    let rows = text_rows(&["ab 日本語".into()], 0, Grid::new(5, 3));
    assert_eq!(rows, ["ab   ", "日本 ", "語   "]);
    assert!(rows.iter().all(|row| row.width() == 5), "{rows:?}");
    // The overflow note is cut to the box by cells too.
    let rows = text_rows(&["日本語のテキスト".repeat(3)], 0, Grid::new(6, 2));
    assert_eq!(rows, ["日本語", "\u{2193} 7 mo"]);
}

#[test]
fn plain_text_measures_and_cuts_whole_graphemes() {
    // A woman technologist: three characters joined into one grapheme of two cells.
    let coder = "\u{1F469}\u{200D}\u{1F4BB}";
    let line = format!("{}{coder}", "a".repeat(70));
    assert_eq!(text_rows(std::slice::from_ref(&line), 0, Grid::new(72, 3)), [line], "72 cells fit a 72-column box");
    // Narrower boxes break and cut between graphemes, never inside the sequence.
    let text = format!("{coder}{coder} a{coder} {}{coder}", "b".repeat(9));
    for cols in 1..=12 {
        for row in text_rows(std::slice::from_ref(&text), 0, Grid::new(cols, 20)) {
            assert_eq!(row.matches('\u{200D}').count() * 2, row.matches(['\u{1F469}', '\u{1F4BB}']).count(), "{cols}: {row:?}");
            assert_eq!(cells(&row), usize::from(cols), "{cols}: {row:?}");
        }
    }
}
