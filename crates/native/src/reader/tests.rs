use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, unbounded};
use yoyomi_core::{At, Cancel, Error, PageKind, Pixels, Result, RgbaImage, Scene};

use super::*;

/// A book of fixed pages. The decode of page `held` waits until `released`
/// is set or its request is cancelled.
struct Pages {
    path: PathBuf,
    pages: Vec<Pixels>,
    held: Option<usize>,
    released: AtomicBool,
}

impl Book for Pages {
    fn path(&self) -> &Path {
        &self.path
    }

    fn title(&self) -> &str {
        "pages"
    }

    fn len(&self) -> usize {
        self.pages.len()
    }

    fn kind(&self, _page: usize) -> PageKind {
        PageKind::Image
    }

    fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
        if self.held == Some(page) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.released.load(Ordering::SeqCst) && !cancel.is_cancelled() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
        }
        cancel.check()?;
        self.pages.get(page).cloned().ok_or_else(|| Error::Format("no such page".into()))
    }
}

fn pages(count: usize, width: u32, height: u32) -> Vec<RgbaImage> {
    (0..count).map(|page| RgbaImage::from_pixel(width, height, [page as u8, 0, 0, 255].into())).collect()
}

fn book(pages: Vec<RgbaImage>, held: Option<usize>) -> Arc<Pages> {
    let pages = pages.into_iter().map(Arc::new).collect();
    Arc::new(Pages { path: PathBuf::from("/books/pages.cbz"), pages, held, released: AtomicBool::new(false) })
}

struct Rig {
    reader: Reader,
    events: Receiver<ViewEvent>,
}

fn rig(book: Arc<Pages>, state: State, (width, height): (u32, u32)) -> Rig {
    let (sender, events) = unbounded();
    let mut reader = Reader::new(Arc::new(PageCache::new(book)), Jobs::new(2), sender, state);
    reader.resize(width, height);
    Rig { reader, events }
}

impl Rig {
    /// The next frame to draw, waiting for the jobs that render it.
    fn frame(&mut self) -> Shown {
        loop {
            if let Some(shown) = self.reader.take_frame() {
                return shown.clone();
            }
            let event = self.events.recv_timeout(Duration::from_secs(10)).expect("a job finishes");
            self.reader.on_view_event(event);
        }
    }

    fn keys(&mut self, keys: &[&str]) -> Option<Action> {
        keys.iter().map(|key| self.reader.key(key)).last().flatten()
    }

    /// Eases webtoon scrolling to its end, drawing every frame it moves.
    fn ease(&mut self) {
        for frames in 0.. {
            assert!(frames < 1000, "the easing stops");
            if !self.reader.moving() {
                return;
            }
            let before = self.reader.state();
            self.reader.tick();
            if self.reader.state() != before {
                self.frame();
            }
        }
    }
}

/// The pixel a frame shows at `x`, `y`, or `None` where it shows nothing.
fn pixel(shown: &Shown, x: u32, y: u32) -> Option<[u8; 4]> {
    shown.scene.layers.iter().find_map(|layer| {
        let Layer::Pixels { image, at } = layer else { return None };
        let (col, row) = (x.checked_sub(u32::from(at.col))?, y.checked_sub(u32::from(at.row))?);
        (col < image.width() && row < image.height()).then(|| image.get_pixel(col, row).0)
    })
}

#[test]
fn keys_count_turn_and_close() {
    let mut rig = rig(book(pages(20, 4, 4), None), State::default(), (40, 40));
    assert_eq!(rig.frame().page, 0);
    rig.keys(&["1", "2", "j"]);
    assert_eq!(rig.reader.state().page, 12);
    rig.keys(&["0", "K"]);
    assert_eq!(rig.reader.state().page, 11, "a leading 0 counts nothing");
    rig.keys(&["G"]);
    assert_eq!(rig.reader.state().page, 19);
    rig.keys(&["g"]);
    assert_eq!(rig.reader.state().page, 0);
    assert_eq!(rig.keys(&["q"]), Some(Action::Clear));
    assert_eq!(rig.keys(&["Q"]), Some(Action::Save));
    assert_eq!(rig.keys(&["f"]), Some(Action::Return));
}

#[test]
fn g_and_big_g_go_to_the_counted_page() {
    let mut rig = rig(book(pages(50, 4, 4), None), State::default(), (40, 40));
    rig.keys(&["4", "2", "G"]);
    assert_eq!(rig.reader.state().page, 41, "42G shows page 42");
    rig.keys(&["j"]);
    assert_eq!(rig.reader.state().page, 42, "G took the count");
    rig.keys(&["3", "g"]);
    assert_eq!(rig.reader.state().page, 2);
    rig.keys(&["j"]);
    assert_eq!(rig.reader.state().page, 3, "g took the count");
    rig.keys(&["G"]);
    assert_eq!(rig.reader.state().page, 49);
    rig.keys(&["g", "9", "9", "g"]);
    assert_eq!(rig.reader.state().page, 49, "a count past the end shows the last page");
}

#[test]
fn spread_turns_stop_on_the_last_pair() {
    let mut rig = rig(book(pages(6, 4, 4), None), State::default(), (40, 40));
    rig.keys(&["s", "j", "j"]);
    assert_eq!(rig.reader.state().page, 4);
    rig.keys(&["j"]);
    assert_eq!(rig.reader.state().page, 4, "pages 4 and 5 are the last pair");
    rig.keys(&["3", "k"]);
    assert_eq!(rig.reader.state().page, 0);
}

#[test]
fn zoom_and_pan_stay_in_bounds() {
    let mut rig = rig(book(pages(2, 4, 4), None), State::default(), (40, 40));
    rig.keys(&["+"; 20]);
    assert_eq!(rig.reader.state().zoom, ZOOM_MAX);
    rig.keys(&["left"; 40]);
    rig.keys(&["down"; 40]);
    assert_eq!(rig.reader.state().center, (0.5 / ZOOM_MAX, 1.0 - 0.5 / ZOOM_MAX));
    rig.keys(&["-"; 20]);
    let state = rig.reader.state();
    assert_eq!((state.zoom, state.center), (1.0, (0.5, 0.5)), "zooming out to the whole page centers it");
    rig.keys(&["right"]);
    assert_eq!(rig.reader.state().center, (0.5, 0.5), "a whole page does not pan");
    rig.keys(&["t", "+"]);
    assert_eq!(rig.reader.state().zoom, 1.0, "webtoon mode does not zoom");
}

#[test]
fn webtoon_keys_scroll_a_viewport_and_stop_at_the_ends() {
    // One page taller than the view keeps its width and scrolls in place.
    let mut rig = rig(book(pages(1, 100, 2000), None), State { webtoon: true, ..State::default() }, (100, 80));
    rig.frame();
    rig.keys(&["j"]);
    rig.ease();
    let offset = rig.reader.state().offset;
    assert!((offset - 80.0 / 2000.0).abs() < 1e-4, "{offset}");
    rig.keys(&["J"]);
    rig.ease();
    let offset = rig.reader.state().offset;
    assert!((offset - 120.0 / 2000.0).abs() < 1e-4, "{offset}");
    rig.keys(&["5", "k"]);
    rig.ease();
    assert_eq!(rig.reader.state().offset, 0.0, "the top stops the scrolling");
    rig.keys(&["G"]);
    rig.frame();
    let offset = rig.reader.state().offset;
    assert!((offset - 1920.0 / 2000.0).abs() < 1e-9, "G shows the end: {offset}");
    rig.keys(&["j"]);
    rig.ease();
    assert!((rig.reader.state().offset - offset).abs() < 1e-9, "the end stops the scrolling");
}

#[test]
fn webtoon_toggles_keep_the_page_and_reset_the_view() {
    let mut rig = rig(book(pages(3, 100, 100), None), State { page: 1, spread: true, zoom: 2.0, ..State::default() }, (100, 80));
    rig.keys(&["t"]);
    let state = rig.reader.state();
    assert!(state.webtoon && !state.spread && state.zoom == 1.0 && state.page == 1, "{state:?}");
    rig.frame();
    rig.keys(&["j"]);
    rig.ease();
    assert_eq!(rig.reader.state().page, 1, "80 of 100 rows down page 1");
    rig.keys(&["J"]);
    rig.ease();
    assert_eq!(rig.reader.state().page, 2);
    rig.keys(&["t"]);
    let state = rig.reader.state();
    assert!(!state.webtoon && state.page == 2 && state.offset == 0.0, "{state:?}");
    assert_eq!(rig.frame().page, 2);
}

#[test]
fn a_trackpad_gesture_turns_one_page_and_a_wheel_notch_one_each() {
    let mut rig = rig(book(pages(10, 4, 4), None), State::default(), (40, 40));
    let start = Instant::now();
    let swipe = |dy: f64, began: bool| Wheel { dy, precise: true, began, gesture: true, scale: 2.0 };
    rig.reader.wheel(swipe(-5.0, true), start);
    rig.reader.wheel(swipe(-5.0, false), start);
    assert_eq!(rig.reader.state().page, 0, "10 points do not turn");
    rig.reader.wheel(swipe(-5.0, false), start);
    rig.reader.wheel(swipe(-50.0, false), start);
    assert_eq!(rig.reader.state().page, 1, "a gesture turns once");
    rig.reader.wheel(swipe(-20.0, false), start + Duration::from_millis(100));
    assert_eq!(rig.reader.state().page, 1);
    rig.reader.wheel(swipe(-20.0, false), start + Duration::from_millis(300));
    assert_eq!(rig.reader.state().page, 2, "a pause starts a new gesture");
    rig.reader.wheel(swipe(30.0, true), start + Duration::from_millis(310));
    assert_eq!(rig.reader.state().page, 1, "upward deltas turn back");

    let notch = Wheel { dy: -1.0, precise: false, began: false, gesture: false, scale: 2.0 };
    rig.reader.wheel(notch, start);
    rig.reader.wheel(Wheel { dy: -1.0, precise: false, began: false, gesture: false, scale: 2.0 }, start);
    assert_eq!(rig.reader.state().page, 3);
}

#[test]
fn the_wheel_scrolls_webtoon_pages_in_device_pixels() {
    let mut rig = rig(book(pages(1, 100, 2000), None), State { webtoon: true, ..State::default() }, (100, 80));
    rig.frame();
    rig.reader.wheel(Wheel { dy: -10.0, precise: true, began: true, gesture: true, scale: 2.0 }, Instant::now());
    rig.reader.wheel(Wheel { dy: -1.0, precise: false, began: false, gesture: false, scale: 2.0 }, Instant::now());
    rig.ease();
    let offset = rig.reader.state().offset;
    assert!((offset - 44.0 / 2000.0).abs() < 1e-4, "20 pixels for 10 points, 24 for a notch: {offset}");
}

#[test]
fn inversion_keeps_the_original_pages_and_the_margins() {
    for mode in ["single", "spread", "webtoon"] {
        let state = State { spread: mode == "spread", webtoon: mode == "webtoon", ..State::default() };
        let mut rig = rig(book(vec![RgbaImage::from_pixel(8, 8, [0, 0, 0, 255].into()); 2], None), state, (32, 16));
        let (points, margin) = if mode == "spread" { (vec![(7, 8), (24, 8)], (16, 0)) } else { (vec![(16, 4)], (0, 0)) };
        for invert in [false, true, false] {
            if rig.reader.state().invert != invert {
                rig.keys(&["i"]);
            }
            let frame = rig.frame();
            let want = if invert { 255 } else { 0 };
            for &(x, y) in &points {
                assert_eq!(pixel(&frame, x, y).map(|pixel| pixel[0]), Some(want), "{mode} inverted {invert}: pixel {x},{y}");
            }
            assert_eq!(pixel(&frame, margin.0, margin.1).map_or(0, |pixel| pixel[0]), 0, "{mode}: inversion brightened the margin");
        }
        let cached = rig.reader.book.cached(0).expect("page 0 stays cached");
        assert_eq!(cached.get_pixel(0, 0).0, [0, 0, 0, 255], "{mode}: inversion changed a cached page");
    }
}

#[test]
fn clicks_map_through_the_painted_crop_and_rotation() {
    for scale in [1_u16, 2] {
        for (quarter_turns, want) in [(0, (0.375, 0.375)), (1, (0.375, 0.625)), (2, (0.625, 0.625)), (3, (0.625, 0.375))] {
            let mut rig = rig(book(pages(10, 4, 4), None), State { page: 9, ..State::default() }, (40, 40));
            // Page 3 shows the middle half of each side, zoomed by 2.
            let image = Arc::new(RgbaImage::new(160 * u32::from(scale), 80 * u32::from(scale)));
            let shown = Shown {
                page: 3,
                scene: Scene { layers: vec![Layer::Pixels { image, at: At { col: 20 * scale, row: 10 * scale } }] },
                errors: Vec::new(),
            };
            let size = Grid::new(200 * scale, 100 * scale);
            rig.reader.painted = Some(Painted { shown, size, webtoon: false, spread: false, quarter_turns, zoom: 2.0, center: (0.5, 0.5) });
            let (page, x, y) = rig.reader.page_point(0.3, 0.3).expect("a point on the page");
            assert!(page == 3 && (x - want.0).abs() < 1e-9 && (y - want.1).abs() < 1e-9, "scale {scale} turns {quarter_turns}: {page} {x} {y}");
            for (fx, fy) in [(0.0, 0.0), (1.0, 1.0), (-0.1, 0.5), (f64::NAN, 0.5)] {
                assert_eq!(rig.reader.page_point(fx, fy), None, "the margin or a bad point maps to no page");
            }
        }
    }
}

#[test]
fn spread_and_webtoon_clicks_find_their_page() {
    let mut spread = rig(book(pages(2, 200, 100), None), State { spread: true, ..State::default() }, (808, 200));
    spread.frame();
    assert_eq!(spread.reader.page_point(0.8, 0.5).map(|(page, ..)| page), Some(1), "the right slot");
    assert_eq!(spread.reader.page_point(0.5, 0.5), None, "the gap between the pages");

    let mut rig = rig(book(pages(2, 200, 100), None), State { webtoon: true, offset: 0.1, ..State::default() }, (200, 150));
    rig.frame();
    let (page, x, y) = rig.reader.page_point(0.1, 0.7).expect("a point on page 1");
    assert!(page == 1 && (x - 0.1).abs() < 1e-9 && (y - 0.15).abs() < 1e-9, "{page} {x} {y}");
}

#[test]
fn a_turn_waiting_for_its_page_keeps_clicks_on_the_painted_one() {
    let pages = book(pages(2, 100, 50), Some(1));
    let mut rig = rig(pages.clone(), State::default(), (100, 80));
    assert_eq!(rig.frame().page, 0);
    rig.keys(&["j"]);
    assert_eq!(rig.reader.state().page, 1, "the turn does not wait for the page");
    assert_eq!(rig.reader.page_point(0.5, 0.5).map(|(page, ..)| page), Some(0));
    pages.released.store(true, Ordering::SeqCst);
    assert_eq!(rig.frame().page, 1);
    assert_eq!(rig.reader.page_point(0.5, 0.5).map(|(page, ..)| page), Some(1));
}

#[test]
fn links_go_to_their_page_with_the_whole_page_in_view() {
    let state = State { spread: true, zoom: 2.0, center: (0.3, 0.3), offset: 0.5, ..State::default() };
    let mut rig = rig(book(pages(4, 4, 4), None), state, (40, 40));
    rig.reader.go_to(3);
    let state = rig.reader.state();
    assert_eq!((state.page, state.zoom, state.center, state.offset), (3, 1.0, (0.5, 0.5), 0.0));
}

#[test]
fn a_failing_page_reports_why() {
    let mut rig = rig(book(pages(1, 4, 4), None), State::default(), (40, 40));
    rig.frame();
    assert_eq!(rig.reader.error(), None);
    rig.reader.state.page = 5;
    rig.reader.show();
    rig.frame();
    assert_eq!(rig.reader.error().as_deref(), Some("page 6: no such page"));
}
