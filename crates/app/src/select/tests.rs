use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};
use crossterm::event::Event as Input;
use crossterm::event::KeyModifiers;
use yoyomi_core::{At, PageKind, Pixels, RgbaImage, Scene};
use yoyomi_formats::Opened;
use yoyomi_render::Kitty;

use super::*;
use crate::app::tests::{app_with, deliver as open_in, keys, run_views};
use crate::app::{App, Event as AppEvent};

/// A book of two pages whose first page carries `text::tests::page`.
pub(crate) struct TextBook;

impl Book for TextBook {
    fn path(&self) -> &Path {
        Path::new("/books/text.pdf")
    }
    fn title(&self) -> &str {
        "text"
    }
    fn len(&self) -> usize {
        2
    }
    fn kind(&self, _page: usize) -> PageKind {
        PageKind::Rendered
    }
    fn page(&self, _page: usize, _cancel: &Cancel) -> Result<Pixels> {
        Ok(Arc::new(RgbaImage::new(4, 2)))
    }
    fn text_layer(&self, page: usize, cancel: &Cancel) -> Result<Option<Arc<TextLayer>>> {
        cancel.check()?;
        Ok((page == 0).then(|| Arc::new(text::tests::page())))
    }
}

thread_local! {
    static OPENED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn record_open(url: &str) -> io::Result<()> {
    OPENED.with(|opened| opened.borrow_mut().push(url.to_owned()));
    Ok(())
}

fn opened() -> Vec<String> {
    OPENED.with(|opened| opened.take())
}

fn select_on(book: Arc<dyn Book>) -> (Select, Receiver<Event>) {
    let (events, received) = unbounded();
    let mut select = Select::new(Arc::new(PageCache::new(book)), Jobs::new(1), events);
    select.session = Session::default();
    select.open = record_open;
    (select, received)
}

/// Page 0 in a slot of 40 x 20 cells from cell (10, 5).
fn view() -> View {
    View { slots: vec![Slot { page: 0, x: 10, y: 5, cols: 40, rows: 20 }], zoom: 1.0, center: (0.5, 0.5) }
}

/// Cells over the words of `text::tests::page`: "visit", and "page" of
/// "next page".
const VISIT: (u16, u16) = (16, 12);
const NEXT_PAGE: (u16, u16) = (21, 16);
/// The slot's top-left cell, and a cell past its bottom-right corner.
const TOP_LEFT: (u16, u16) = (10, 5);
const PAST_BOTTOM_RIGHT: (u16, u16) = (60, 30);

fn mouse(select: &mut Select, kind: MouseEventKind, (column, row): (u16, u16)) -> Outcome {
    select.mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }, &view())
}

fn click(select: &mut Select, at: (u16, u16)) -> Outcome {
    mouse(select, MouseEventKind::Down(MouseButton::Left), at);
    mouse(select, MouseEventKind::Up(MouseButton::Left), at)
}

fn drag(select: &mut Select, from: (u16, u16), to: (u16, u16)) -> Outcome {
    mouse(select, MouseEventKind::Down(MouseButton::Left), from);
    mouse(select, MouseEventKind::Drag(MouseButton::Left), to);
    mouse(select, MouseEventKind::Up(MouseButton::Left), to)
}

/// Hands the next loaded layer to `select`.
fn deliver(select: &mut Select, received: &Receiver<Event>) -> Outcome {
    match received.recv_timeout(Duration::from_secs(5)).expect("a layer loads") {
        Event::TextLayer { page, layer, .. } => select.on_layer(page, layer, &view()),
        _ => panic!("not a text layer"),
    }
}

/// Selection on `TextBook` with page 0's layer loaded.
fn loaded() -> Select {
    let (mut select, received) = select_on(Arc::new(TextBook));
    mouse(&mut select, MouseEventKind::Moved, TOP_LEFT);
    deliver(&mut select, &received);
    select
}

#[test]
fn hovering_names_links_once_the_layer_loads() {
    let (mut select, received) = select_on(Arc::new(TextBook));
    mouse(&mut select, MouseEventKind::Moved, VISIT);
    assert_eq!(select.hover, "", "the layer is still loading");
    assert_eq!(deliver(&mut select, &received), Outcome::default());
    assert_eq!(select.hover, "https://example.com/a", "the layer names the link under the resting mouse; tracking parameters drop out");
    mouse(&mut select, MouseEventKind::Moved, NEXT_PAGE);
    assert_eq!(select.hover, "p.2");
    mouse(&mut select, MouseEventKind::Moved, (0, 0));
    assert_eq!(select.hover, "");
    mouse(&mut select, MouseEventKind::Moved, VISIT);
    select.on_frame(&[0]);
    assert_eq!(select.hover, "", "a new frame may move the link");
}

#[test]
fn clicks_follow_links() {
    let mut select = loaded();
    assert_eq!(click(&mut select, NEXT_PAGE), Outcome { status: Some(String::new()), go_to: Some(1), ..Outcome::default() });
    let outcome = click(&mut select, VISIT);
    assert_eq!(outcome.status.as_deref(), Some("→ https://example.com/a"));
    assert_eq!(opened(), ["https://example.com/a"]);
    assert_eq!(click(&mut select, TOP_LEFT).go_to, None, "plain words lead nowhere");
    assert!(opened().is_empty() && select.clipboard.is_none());
}

#[test]
fn right_clicks_copy_web_links() {
    let mut select = loaded();
    let outcome = mouse(&mut select, MouseEventKind::Down(MouseButton::Right), VISIT);
    assert_eq!(outcome.status.as_deref(), Some("copied https://example.com/a"));
    assert_eq!(select.clipboard.take(), Some(osc52("https://example.com/a")));
    assert!(opened().is_empty());
    mouse(&mut select, MouseEventKind::Down(MouseButton::Right), NEXT_PAGE);
    assert_eq!(select.clipboard, None, "page links have nothing to copy");
}

#[test]
fn drags_select_highlight_and_copy() {
    let mut select = loaded();
    mouse(&mut select, MouseEventKind::Down(MouseButton::Left), TOP_LEFT);
    assert!(select.highlights(0).is_empty());
    let moved = mouse(&mut select, MouseEventKind::Drag(MouseButton::Left), PAST_BOTTOM_RIGHT);
    assert!(moved.redraw, "the drag redraws the highlights");
    assert_eq!(select.highlights(0).len(), 3);
    assert!(select.highlights(1).is_empty(), "highlights stay on their page");
    assert_eq!(mouse(&mut select, MouseEventKind::Drag(MouseButton::Left), PAST_BOTTOM_RIGHT), Outcome::default(), "no move, no redraw");
    let released = mouse(&mut select, MouseEventKind::Up(MouseButton::Left), PAST_BOTTOM_RIGHT);
    assert_eq!(released.status.as_deref(), Some("copied 36 characters"));
    assert_eq!(select.clipboard.take(), Some(osc52("Hello reader\nvisit example\nnext page")));
    assert_eq!(select.highlights(0).len(), 3, "the highlights stay after the copy");
    assert!(!mouse(&mut select, MouseEventKind::Down(MouseButton::Left), (0, 0)).redraw, "a press beside the page keeps them");
    assert!(mouse(&mut select, MouseEventKind::Down(MouseButton::Left), TOP_LEFT).redraw, "a press on the page clears them");
    assert!(select.highlights(0).is_empty());
}

#[test]
fn actions_wait_for_their_layer() {
    let (mut select, received) = select_on(Arc::new(TextBook));
    assert_eq!(drag(&mut select, TOP_LEFT, PAST_BOTTOM_RIGHT).status.as_deref(), Some("loading text…"));
    let outcome = deliver(&mut select, &received);
    assert_eq!(outcome, Outcome { status: Some("copied 36 characters".into()), redraw: true, go_to: None });
    assert_eq!(select.clipboard, Some(osc52("Hello reader\nvisit example\nnext page")));

    let (mut select, received) = select_on(Arc::new(TextBook));
    assert_eq!(click(&mut select, VISIT).status.as_deref(), Some("loading text…"));
    assert!(opened().is_empty());
    assert_eq!(deliver(&mut select, &received).status.as_deref(), Some("→ https://example.com/a"));
    assert_eq!(opened(), ["https://example.com/a"]);

    let (mut select, received) = select_on(Arc::new(TextBook));
    mouse(&mut select, MouseEventKind::Down(MouseButton::Right), VISIT);
    deliver(&mut select, &received);
    assert_eq!(select.clipboard, Some(osc52("https://example.com/a")));
}

#[test]
fn newer_input_drops_a_waiting_action() {
    let (mut select, received) = select_on(Arc::new(TextBook));
    click(&mut select, VISIT);
    select.interrupt();
    assert_eq!(deliver(&mut select, &received), Outcome::default());
    assert!(opened().is_empty() && select.clipboard.is_none());

    let (mut select, received) = select_on(Arc::new(TextBook));
    mouse(&mut select, MouseEventKind::Down(MouseButton::Right), VISIT);
    mouse(&mut select, MouseEventKind::Down(MouseButton::Left), (0, 0));
    assert_eq!(deliver(&mut select, &received), Outcome { status: Some(String::new()), ..Outcome::default() }, "a press drops it too");
    assert!(select.clipboard.is_none());

    let (mut select, received) = select_on(Arc::new(TextBook));
    mouse(&mut select, MouseEventKind::Down(MouseButton::Right), VISIT);
    select.on_frame(&[1]);
    assert!(select.waiting.is_none() && select.loading.is_empty(), "a page that left drops its action and its load");
    drop(received);
}

#[test]
fn interrupted_presses_and_drags_do_nothing_on_release() {
    let mut select = loaded();
    mouse(&mut select, MouseEventKind::Down(MouseButton::Left), VISIT);
    select.interrupt();
    assert_eq!(mouse(&mut select, MouseEventKind::Up(MouseButton::Left), VISIT), Outcome::default());
    assert!(opened().is_empty(), "the link of the page that left stays closed");

    mouse(&mut select, MouseEventKind::Down(MouseButton::Left), TOP_LEFT);
    mouse(&mut select, MouseEventKind::Drag(MouseButton::Left), PAST_BOTTOM_RIGHT);
    select.interrupt();
    assert_eq!(mouse(&mut select, MouseEventKind::Drag(MouseButton::Left), TOP_LEFT), Outcome::default(), "the drag ended");
    assert_eq!(mouse(&mut select, MouseEventKind::Up(MouseButton::Left), PAST_BOTTOM_RIGHT), Outcome::default());
    assert!(select.clipboard.is_none());
}

#[test]
fn layers_of_pages_that_left_stop_loading() {
    let (mut select, received) = select_on(Arc::new(TextBook));
    mouse(&mut select, MouseEventKind::Moved, VISIT);
    select.on_frame(&[1]);
    assert!(select.loading.is_empty());
    // The cancelled job may have run already; then its layer counts.
    if let Ok(Event::TextLayer { page, layer, .. }) = received.recv_timeout(Duration::from_millis(200)) {
        select.on_layer(page, layer, &view());
    }
    mouse(&mut select, MouseEventKind::Moved, VISIT);
    if select.layers.is_empty() {
        deliver(&mut select, &received);
    }
    mouse(&mut select, MouseEventKind::Moved, VISIT);
    assert_eq!(select.hover, "https://example.com/a", "moving over the page again loads it");
}

#[test]
fn cells_map_through_the_zoom_crop() {
    let view = View { zoom: 2.0, center: (0.75, 0.25), ..view() };
    let close = |(x, y): (f64, f64), (want_x, want_y): (f64, f64)| (x - want_x).abs() < 1e-9 && (y - want_y).abs() < 1e-9;
    let hit = view.hit(10, 5).unwrap();
    assert!(close((hit.x, hit.y), (0.5 + 0.5 / 40.0 / 2.0, 0.5 / 20.0 / 2.0)), "{hit:?}");
    assert!(view.hit(9, 5).is_none() && view.hit(50, 5).is_none(), "cells beside the page");
    let snapped = view.hit_in(view.slots[0], 0, 100);
    assert!(close((snapped.x, snapped.y), (hit.x, 0.5 - 0.5 / 20.0 / 2.0)), "a drag past the page snaps to its edge: {snapped:?}");
}

#[test]
fn slots_follow_the_frames_of_a_spread() {
    let image = ImageBox { x: 1, y: 1, cols: 78, rows: 21 };
    let kitty = |col| Layer::KittyRows { id: yoyomi_core::ImageId(1), cols: 38, src_row: 0, rows: 30, at: At { col, row: 0 } };
    let text = Layer::Text { lines: vec!["text".to_owned()].into(), at: At::default() };
    let shown = Shown { page: 4, scene: Scene { layers: vec![kitty(0), kitty(39), text] }, errors: Vec::new() };
    assert_eq!(
        slots(&shown, image, 24, |_| true),
        [Slot { page: 4, x: 1, y: 1, cols: 38, rows: 21 }, Slot { page: 5, x: 40, y: 1, cols: 38, rows: 21 }]
    );
    assert_eq!(slots(&shown, image, 24, |page| page == 5), [Slot { page: 5, x: 40, y: 1, cols: 38, rows: 21 }]);
    let wide = Layer::Cells { grid: Grid::new(77, 21), colors: Arc::from([]), at: At { col: 0, row: 0 } };
    let narrow = Layer::Cells { grid: Grid::new(1, 21), colors: Arc::from([]), at: At { col: 38, row: 0 } };
    let single = Shown { page: 4, scene: Scene { layers: vec![wide, narrow] }, errors: Vec::new() };
    assert_eq!(slots(&single, image, 24, |_| true).iter().map(|slot| slot.page).collect::<Vec<_>>(), [4, 4], "a centered page");
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pdf").join(name)
}

pub(crate) fn have(tools: &[&str]) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let missing = tools.iter().find(|tool| !std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()));
    if let Some(tool) = missing {
        eprintln!("skipped: {tool} is not installed");
    }
    missing.is_none()
}

#[test]
fn pdf_text_layers_select_and_link() {
    if !have(&["pdfinfo", "pdftotext", "pdftohtml"]) {
        return;
    }
    for name in ["linked.pdf", "cropped.pdf"] {
        let Ok(Opened::Book(book)) = yoyomi_formats::open(&fixture(name)) else { panic!("{name} did not open") };
        let layer = book.text_layer(0, &Cancel::new()).unwrap().unwrap();
        let middle = |line: usize, word: usize| {
            let (line, b) = (&layer.lines[line], layer.lines[line].boxes[word]);
            ((b.x0 + b.x1) / 2.0, (line.top + line.bottom) / 2.0)
        };
        let ((x, y), (page_x, page_y)) = (middle(1, 0), middle(2, 1));
        assert_eq!(link_at(&layer, x, y), Some(&Link::Url("https://example.com/a?utm_source=pdf".into())), "{name}");
        assert_eq!(link_at(&layer, page_x, page_y), Some(&Link::Page(1)), "{name}");
        let (x, y) = middle(0, 0);
        assert_eq!(link_at(&layer, x, y), None, "{name}: plain words");
        let (text, rects) = select(&layer, (0.0, 0.0), (1.0, 1.0));
        assert_eq!(text, "Hello reader\nvisit example\nnext page", "{name}");
        assert_eq!(rects.len(), 3);
        assert!(rects[0].x0 > 0.0 && rects[0].y1 > rects[0].y0 && rects[2].y1 <= 1.0, "{name}: {rects:?}");
        let (x, y) = middle(0, 1);
        assert_eq!(select(&layer, (x - 0.05, y), (1.0, 1.0)).0, "reader\nvisit example\nnext page", "{name}");
        let second = book.text_layer(1, &Cancel::new()).unwrap().unwrap();
        assert_eq!(select(&second, (0.0, 0.0), (1.0, 1.0)).0, "Second");
    }
}

/// A kitty frame of `page` in a slot of 40 x 20 cells at the body's corner.
fn frame(page: usize) -> yoyomi_view::Update {
    let layer = Layer::KittyRows { id: yoyomi_core::ImageId(1), cols: 40, src_row: 0, rows: 20, at: At::default() };
    yoyomi_view::Update { uploads: Vec::new(), shown: Some(Shown { page, scene: Scene { layers: vec![layer] }, errors: Vec::new() }) }
}

/// A kitty reader of `TextBook` with page 0 on screen and its layer loaded.
fn text_reader() -> App {
    let (mut app, received) = app_with(Arc::new(Kitty::new(yoyomi_core::CellSize::FALLBACK, false)), 100, 40);
    open_in(&mut app, 0, "/books/text.pdf", Ok(Arc::new(TextBook)));
    app.apply(frame(0));
    input(&mut app, MouseEventKind::Moved, on_page(VISIT));
    hover_after_load(&mut app, &received);
    app
}

/// A kitty reader of `TextBook` with the whole of page 0 selected.
pub(crate) fn selecting_reader() -> App {
    let mut app = text_reader();
    input(&mut app, MouseEventKind::Down(MouseButton::Left), on_page(TOP_LEFT));
    input(&mut app, MouseEventKind::Drag(MouseButton::Left), on_page(PAST_BOTTOM_RIGHT));
    app
}

/// Hands `app` what its jobs send until the status bar names a link.
fn hover_after_load(app: &mut App, received: &Receiver<AppEvent>) {
    while !app.status_left().contains('→') {
        app.on_event(received.recv_timeout(Duration::from_secs(5)).expect("a layer loads"));
    }
}

/// The body starts at cell (0, 1), so the page's cells sit one cell down
/// from `view`'s.
fn on_page((x, y): (u16, u16)) -> (u16, u16) {
    (x - 10, y - 4)
}

fn input(app: &mut App, kind: MouseEventKind, (column, row): (u16, u16)) {
    app.on_event(AppEvent::Input(Input::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })));
}

#[test]
fn the_reader_wires_the_mouse_to_its_pages() {
    let mut app = text_reader();
    input(&mut app, MouseEventKind::Moved, on_page(VISIT));
    assert!(app.status_left().ends_with("  ·  → https://example.com/a"), "{}", app.status_left());
    input(&mut app, MouseEventKind::Down(MouseButton::Right), on_page(VISIT));
    assert_eq!(app.take_clipboard(), Some(osc52("https://example.com/a")));
    assert!(app.status_left().contains("copied https://example.com/a"));
    input(&mut app, MouseEventKind::Down(MouseButton::Left), on_page(NEXT_PAGE));
    input(&mut app, MouseEventKind::Up(MouseButton::Left), on_page(NEXT_PAGE));
    assert!(app.title(0).starts_with("text  2/2"), "{}", app.title(0));
    keys(&mut app, "k");
    input(&mut app, MouseEventKind::Down(MouseButton::Left), on_page(NEXT_PAGE));
    keys(&mut app, "jk");
    input(&mut app, MouseEventKind::Up(MouseButton::Left), on_page(NEXT_PAGE));
    assert!(app.title(0).starts_with("text  1/2"), "a press before page turns follows no link: {}", app.title(0));
    input(&mut app, MouseEventKind::Down(MouseButton::Left), on_page(NEXT_PAGE));
    input(&mut app, MouseEventKind::ScrollDown, on_page(NEXT_PAGE));
    input(&mut app, MouseEventKind::ScrollUp, on_page(NEXT_PAGE));
    input(&mut app, MouseEventKind::Up(MouseButton::Left), on_page(NEXT_PAGE));
    assert!(app.title(0).starts_with("text  1/2"), "nor before wheel turns: {}", app.title(0));
    keys(&mut app, "R");
    input(&mut app, MouseEventKind::Moved, on_page(VISIT));
    assert!(!app.status_left().contains('→'), "turned pages map no cells");
}

#[test]
fn a_resize_ends_a_drag() {
    let mut app = text_reader();
    input(&mut app, MouseEventKind::Down(MouseButton::Left), on_page(TOP_LEFT));
    input(&mut app, MouseEventKind::Drag(MouseButton::Left), on_page(PAST_BOTTOM_RIGHT));
    app.on_event(AppEvent::Input(Input::Resize(60, 20)));
    input(&mut app, MouseEventKind::Drag(MouseButton::Left), on_page(VISIT));
    input(&mut app, MouseEventKind::Up(MouseButton::Left), on_page(VISIT));
    assert_eq!(app.take_clipboard(), None, "the release after a resize copies nothing");
}

#[test]
fn hovers_map_through_the_zoom_of_the_shown_frame() {
    let mut app = text_reader();
    keys(&mut app, "+");
    input(&mut app, MouseEventKind::Moved, on_page(VISIT));
    assert!(app.status_left().ends_with("  ·  → https://example.com/a"), "the old frame still shows: {}", app.status_left());
    app.apply(frame(0));
    input(&mut app, MouseEventKind::Moved, on_page(VISIT));
    assert!(!app.status_left().contains('→'), "the zoomed frame puts the gap after \"visit\" there: {}", app.status_left());
}

/// A PDF of two tall pages that both carry `text::tests::page`.
struct TwoTextPages;

impl Book for TwoTextPages {
    fn path(&self) -> &Path {
        Path::new("/books/two.pdf")
    }
    fn title(&self) -> &str {
        "two"
    }
    fn len(&self) -> usize {
        2
    }
    fn kind(&self, _page: usize) -> PageKind {
        PageKind::Rendered
    }
    fn page(&self, _page: usize, _cancel: &Cancel) -> Result<Pixels> {
        Ok(Arc::new(RgbaImage::new(400, 600)))
    }
    fn text_layer(&self, _page: usize, _cancel: &Cancel) -> Result<Option<Arc<TextLayer>>> {
        Ok(Some(Arc::new(text::tests::page())))
    }
}

/// The first column, width, first row and height of each run of kitty
/// placeholder columns on screen: one per page drawn.
fn drawn_pages(app: &App) -> Vec<(u16, u16, u16, u16)> {
    let (buf, _) = crate::draw::tests::render(app);
    let placeholder = |x: u16, y: u16| buf[(x, y)].symbol().starts_with('\u{10EEEE}');
    let rows: Vec<u16> = (0..app.rows).filter(|&y| (0..app.cols).any(|x| placeholder(x, y))).collect();
    let mut pages: Vec<(u16, u16, u16, u16)> = Vec::new();
    for x in (0..app.cols).filter(|&x| rows.iter().any(|&y| placeholder(x, y))) {
        match pages.last_mut() {
            Some((x0, cols, ..)) if *x0 + *cols == x => *cols += 1,
            _ => pages.push((x, 1, rows[0], rows.len() as u16)),
        }
    }
    pages
}

#[test]
fn the_pointer_finds_the_word_drawn_under_it_on_pages_and_spreads() {
    let (events, received) = unbounded();
    let (view_events, views) = unbounded();
    let resident = Arc::new(yoyomi_view::Resident::new(yoyomi_core::ImageIds::with_base(0x100), false));
    let renderer = Arc::new(Kitty::new(yoyomi_core::CellSize::FALLBACK, false));
    let positions = yoyomi_state::Positions::load_from(None);
    let mut app = App::new(renderer, yoyomi_render::Terminal::default(), resident, positions, events, view_events, (80, 24));
    open_in(&mut app, 0, "/books/two.pdf", Ok(Arc::new(TwoTextPages)));
    run_views(&mut app, &views, |app| app.shown(0).is_some());
    // "visit" spans 0.1 to 0.2 of the width on the line from 0.3 to 0.4 of
    // the height: the pointer finds it on the first cell whose middle lies on
    // it, and nothing on the cell before.
    let point_at_visit = |app: &mut App, (x, cols, y, rows): (u16, u16, u16, u16)| {
        let col = x + (0.1 * f64::from(cols) - 0.5).ceil() as u16;
        let row = y + (0.35 * f64::from(rows)) as u16;
        input(app, MouseEventKind::Moved, (col, row));
        hover_after_load(app, &received);
        assert!(app.status_left().ends_with("  ·  → https://example.com/a"), "at {x}: {}", app.status_left());
        input(app, MouseEventKind::Moved, (col - 1, row));
        assert!(!app.status_left().contains('→'), "left of the word at {x}: {}", app.status_left());
    };

    // One page centers in the 80 x 22 body below the title.
    let [page] = drawn_pages(&app)[..] else { panic!("one page: {:?}", drawn_pages(&app)) };
    let (x, cols, y, rows) = page;
    assert_eq!((x, y), ((80 - cols) / 2, 1 + (22 - rows) / 2), "{cols} x {rows}");
    point_at_visit(&mut app, page);

    keys(&mut app, "s");
    run_views(&mut app, &views, |app| app.shown(0).is_some_and(|shown| shown.scene.layers.len() == 2));
    // Each page of the spread fits 37 columns and centers in its half: 39
    // columns from 0, and 40 from 40.
    let [left, right] = drawn_pages(&app)[..] else { panic!("two pages: {:?}", drawn_pages(&app)) };
    let (_, cols, y, rows) = left;
    assert!(cols <= 37 && rows <= 21, "{cols} x {rows}");
    assert_eq!((left.0, right.0, y), ((39 - cols) / 2, 40 + (40 - cols) / 2, 1 + (22 - rows) / 2));
    point_at_visit(&mut app, left);
    point_at_visit(&mut app, right);
}

#[test]
fn a_frame_of_another_page_ends_a_drag() {
    let mut app = text_reader();
    input(&mut app, MouseEventKind::Down(MouseButton::Left), on_page(TOP_LEFT));
    input(&mut app, MouseEventKind::Drag(MouseButton::Left), on_page(PAST_BOTTOM_RIGHT));
    app.apply(frame(1));
    app.apply(frame(0));
    input(&mut app, MouseEventKind::Up(MouseButton::Left), on_page(PAST_BOTTOM_RIGHT));
    assert_eq!(app.take_clipboard(), None, "page 0 left and came back, so the drag ended");
}

#[test]
fn releases_on_pages_that_left_do_nothing() {
    let mut select = loaded();
    mouse(&mut select, MouseEventKind::Down(MouseButton::Left), VISIT);
    let other = View { slots: vec![Slot { page: 1, ..view().slots[0] }], ..view() };
    let up = MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column: VISIT.0, row: VISIT.1, modifiers: KeyModifiers::NONE };
    assert_eq!(select.mouse(up, &other), Outcome::default());
    assert!(opened().is_empty(), "the cached link of page 0 stays closed");
}

#[test]
fn the_pane_under_the_pointer_takes_the_mouse() {
    let (mut app, received) = app_with(Arc::new(Kitty::new(yoyomi_core::CellSize::FALLBACK, false)), 100, 40);
    app.split = true;
    open_in(&mut app, 0, "/books/text.pdf", Ok(Arc::new(TextBook)));
    open_in(&mut app, 1, "/books/text.pdf", Ok(Arc::new(TextBook)));
    keys(&mut app, "w");
    app.apply(frame(0));
    keys(&mut app, "w");
    app.apply(frame(0));
    // The right pane's body starts 50 columns right of the left one's.
    let right = |at| {
        let (x, y) = on_page(at);
        (x + 50, y)
    };
    input(&mut app, MouseEventKind::Moved, right(VISIT));
    hover_after_load(&mut app, &received);
    input(&mut app, MouseEventKind::Down(MouseButton::Left), right(NEXT_PAGE));
    input(&mut app, MouseEventKind::Up(MouseButton::Left), right(NEXT_PAGE));
    assert!(app.title(1).starts_with("text  2/2") && app.title(0).starts_with("text  1/2"), "{} | {}", app.title(0), app.title(1));
    assert_eq!(app.active, 1, "the click makes the pane it is in active");

    input(&mut app, MouseEventKind::Moved, on_page(TOP_LEFT));
    assert!(!app.status_left().contains('→'), "the right pane's hover cleared: {}", app.status_left());
    input(&mut app, MouseEventKind::Moved, on_page(VISIT));
    hover_after_load(&mut app, &received);
    input(&mut app, MouseEventKind::Down(MouseButton::Right), on_page(VISIT));
    assert_eq!(app.take_clipboard(), Some(osc52("https://example.com/a")));
    assert_eq!(app.take_clipboard(), None);
}

#[test]
fn native_links_come_from_the_text_layer_without_tracking() {
    let layer = text::tests::page();
    let middle = |line: usize, word: usize| {
        let (line, b) = (&layer.lines[line], layer.lines[line].boxes[word]);
        ((b.x0 + b.x1) / 2.0, (line.top + line.bottom) / 2.0)
    };
    let (x, y) = middle(1, 0);
    assert_eq!(native_link(&TextBook, 0, x, y), Some(Link::Url("https://example.com/a".into())));
    let (x, y) = middle(2, 1);
    assert_eq!(native_link(&TextBook, 0, x, y), Some(Link::Page(1)));
    assert_eq!(native_link(&TextBook, 1, x, y), None, "a page without a text layer");
}
