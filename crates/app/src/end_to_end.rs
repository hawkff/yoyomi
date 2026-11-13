//! The two speedups through the app's own keys and frame writer, with the
//! real pager, strip and kitty renderer on books from `testdata/`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, at, never, select, unbounded};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::{TerminalOptions, Viewport};
use yoyomi_core::{CellSize, ImageIds, Layer};
use yoyomi_render::{Kitty, Terminal};
use yoyomi_state::Positions;
use yoyomi_view::{Resident, ViewEvent};

use crate::app::App;
use crate::app::tests::keys;
use crate::draw::tests::{Recorder, render};
use crate::draw::write_frame;

/// The longest any test waits for the views.
const WAIT: Duration = Duration::from_secs(20);

/// An 80 x 24 kitty reader on a book, with the bytes it wrote.
struct Session {
    app: App,
    resident: Arc<Resident>,
    view_received: Receiver<ViewEvent>,
    terminal: ratatui::Terminal<CrosstermBackend<Recorder>>,
    written: Recorder,
    /// How much of `written` earlier frames returned.
    seen: usize,
    /// The ids of the kitty uploads written so far, in order.
    uploaded: Vec<u32>,
}

impl Session {
    fn open(fixture: &str) -> Self {
        let (events, received) = unbounded();
        let (view_events, view_received) = unbounded();
        let resident = Arc::new(Resident::new(ImageIds::with_base(0x100), false));
        let renderer = Arc::new(Kitty::new(CellSize::FALLBACK, false));
        let mut app = App::new(renderer, Terminal::default(), resident.clone(), Positions::load_from(None), events, view_events, (80, 24));
        app.open_books(&[Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(fixture)]);
        app.on_event(received.recv_timeout(WAIT).expect("the book opens"));
        let written = Recorder::default();
        let options = TerminalOptions { viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)) };
        let terminal = ratatui::Terminal::with_options(CrosstermBackend::new(written.clone()), options).unwrap();
        let mut session = Session { app, resident, view_received, terminal, written, seen: 0, uploaded: Vec::new() };
        session.frame();
        session
    }

    /// Writes a frame, as the event loop does after every event, and returns
    /// its bytes.
    fn frame(&mut self) -> String {
        self.uploaded.extend(self.app.uploads.iter().map(|upload| upload.id.0));
        write_frame(&mut self.terminal, &mut self.app).unwrap();
        let text = self.written.text();
        let frame = text[self.seen..].to_owned();
        self.seen = text.len();
        frame
    }

    /// Presses each key in turn and returns the frames written.
    fn press(&mut self, keys_pressed: &str) -> String {
        let mut frames = String::new();
        for key in keys_pressed.chars() {
            keys(&mut self.app, &key.to_string());
            frames += &self.frame();
        }
        frames
    }

    /// Runs the event loop on finished jobs and scroll timers until `done`,
    /// and returns the frames written.
    fn until(&mut self, done: impl Fn(&Session) -> bool) -> String {
        let limit = Instant::now() + WAIT;
        let mut frames = String::new();
        while !done(self) {
            let timer = self.app.deadline().map_or_else(never, at);
            select! {
                recv(self.view_received) -> event => self.app.on_view_event(event.unwrap()),
                recv(timer) -> _ => self.app.on_timer(Instant::now()),
                recv(at(limit)) -> _ => panic!("the views did not settle in {WAIT:?}; uploads so far: {:?}", self.uploaded),
            }
            frames += &self.frame();
        }
        frames
    }

    /// Runs the loop until scrolling stops.
    fn settle_scroll(&mut self) -> String {
        self.until(|session| session.app.deadline().is_none())
    }

    /// The kitty ids whose placeholder cells the screen shows.
    fn on_screen(&self) -> BTreeSet<u32> {
        let (buf, _) = render(&self.app);
        let placeholders = buf.content.iter().filter(|cell| cell.symbol().starts_with('\u{10EEEE}'));
        placeholders
            .filter_map(|cell| match cell.fg {
                Color::Rgb(r, g, b) => Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)),
                _ => None,
            })
            .collect()
    }

    /// The first image row the top of the scene shows.
    fn top_row(&self) -> u16 {
        match self.app.shown(0).and_then(|shown| shown.scene.layers.first()) {
            Some(Layer::KittyRows { src_row, .. }) => *src_row,
            other => panic!("not a kitty scene: {other:?}"),
        }
    }
}

fn uploads(frames: &str) -> usize {
    frames.matches("a=T").count()
}

fn set(ids: &[u32]) -> BTreeSet<u32> {
    ids.iter().copied().collect()
}

#[test]
fn a_turn_to_the_readied_page_writes_no_upload() {
    let mut session = Session::open("nested.cbz");
    // The first page goes out with its frame, the second once it renders.
    session.until(|session| session.uploaded.len() == 2);
    assert_eq!(session.on_screen(), set(&session.uploaded[..1]));

    let frame = session.press("j");
    assert_eq!(uploads(&frame), 0, "{frame:?}");
    assert!(frame.contains('\u{10EEEE}'), "the turn writes page 2's placeholder cells");
    assert_eq!(session.app.shown(0).map(|shown| shown.page), Some(1));
    assert_eq!(session.on_screen(), set(&session.uploaded[1..2]));

    // Page 3 readies in the background; page 1 is still in the terminal.
    session.until(|session| session.uploaded.len() == 3);
    let frame = session.press("k");
    assert_eq!(uploads(&frame), 0, "{frame:?}");
    assert_eq!(session.on_screen(), set(&session.uploaded[..1]));

    // Closing the book gives every id back, lowest first.
    session.app.finish();
    let leased: Vec<u32> = (0..3).map(|_| session.resident.lease().id.0).collect();
    assert_eq!(set(&leased), set(&session.uploaded));
}

#[test]
fn a_turn_to_the_readied_spread_writes_no_upload() {
    let mut session = Session::open("nested.cbz");
    session.until(|session| session.uploaded.len() == 2);
    // Spread 1-2 renders for its narrower slots, then spread 3-4 readies.
    session.press("s");
    session.until(|session| session.uploaded.len() == 6);
    assert_eq!(session.on_screen(), set(&session.uploaded[2..4]));

    let frame = session.press("j");
    assert_eq!(uploads(&frame), 0, "{frame:?}");
    assert_eq!(session.on_screen(), set(&session.uploaded[4..6]));

    // Page 5 readies alone; spread 1-2 is still in the terminal.
    session.until(|session| session.uploaded.len() == 7);
    let frame = session.press("k");
    assert_eq!(uploads(&frame), 0, "{frame:?}");
    assert_eq!(session.on_screen(), set(&session.uploaded[2..4]));
}

#[test]
fn webtoon_scrolling_uploads_only_new_bands() {
    let mut session = Session::open("webtoon.cbz");
    session.until(|session| session.uploaded.len() == 2);
    // The viewport's band, then the band below it.
    session.press("t");
    session.until(|session| session.uploaded.len() == 4 && session.app.deadline().is_none());
    let bands = set(&session.uploaded[2..4]);
    assert!(session.on_screen().is_subset(&bands));
    assert_eq!(session.top_row(), 0);

    // Half a viewport down and back stays inside the two bands.
    for (key, moved) in [("J", true), ("K", false)] {
        let frames = session.press(key) + &session.settle_scroll();
        assert_eq!(uploads(&frames), 0, "{key}: {frames:?}");
        assert!(frames.contains('\u{10EEEE}'), "{key} writes the scrolled cells");
        assert_eq!(session.top_row() != 0, moved, "{key}");
        assert!(session.on_screen().is_subset(&bands));
    }

    // A viewport further brings the third band into view: exactly its upload.
    session.press("J");
    session.settle_scroll();
    let frames = session.press("j") + &session.settle_scroll();
    assert_eq!(uploads(&frames), 1, "{frames:?}");
    assert_eq!(session.uploaded.len(), 5);
    assert!(session.on_screen().contains(&session.uploaded[4]));
}
