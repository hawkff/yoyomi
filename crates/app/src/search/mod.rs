//! The search line that `/` opens, and the status texts of a search.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use yoyomi_search::Search;

/// The search line's input, styled as the pane title.
pub const INPUT_STYLE: Style = Style::new().add_modifier(Modifier::BOLD).fg(Color::Indexed(212));

/// The hint after the search line's input.
pub const LINE_HINT: &str = "  OCR search · enter run · esc cancel";

/// What a key does to the search line.
#[derive(Debug, PartialEq, Eq)]
pub enum LineKey {
    /// The line stays open.
    Edit,
    /// The line closes without a search.
    Close,
    /// The line closes and searches for the trimmed term.
    Run(String),
}

/// Applies `key` to the search line's input `line`.
pub fn edit(line: &mut String, key: KeyEvent) -> LineKey {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => LineKey::Close,
        KeyCode::Char('c') if ctrl => LineKey::Close,
        KeyCode::Enter => match line.trim() {
            "" => LineKey::Close,
            term => LineKey::Run(term.to_owned()),
        },
        KeyCode::Backspace => {
            line.pop();
            LineKey::Edit
        }
        KeyCode::Char(c) if !ctrl => {
            line.push(c);
            LineKey::Edit
        }
        _ => LineKey::Edit,
    }
}

/// The progress the status bar shows while `search` runs.
pub fn progress(search: &Search) -> Option<String> {
    search.running().then(|| format!("OCR {}/{} ({} hits)", search.scanned(), search.total(), search.hits().len()))
}

/// The status once `search` has read every page.
pub fn finished(search: &Search) -> String {
    format!("search done: {} hit(s) for {:?} · n/p to jump{}", search.hits().len(), search.term(), not_searched(search))
}

/// The note on pages `search` could not read, which ends its final status.
pub fn not_searched(search: &Search) -> String {
    match search.skipped() {
        0 => String::new(),
        skipped => format!(" · {skipped} page(s) not searched"),
    }
}

/// What `n` and `p` say when the reader has no search.
pub const NO_SEARCH: &str = "no search — press / first";

/// The hit `n` (`forward`) or `p` goes to from `page` in a search with a
/// term, with its status, or the status that says why there is none.
pub fn hit(search: &Search, page: usize, forward: bool) -> Result<(usize, String), String> {
    match search.hit_from(page, forward) {
        Some(hit) => Ok((hit, format!("hit {} p.{} ({} total)", search.term(), hit + 1, search.hits().len()))),
        None if search.running() => Err(format!("OCR {}/{} · no hits yet", search.scanned(), search.total())),
        None => Err(format!("no hits for {:?}", search.term())),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use crossbeam_channel::Receiver;
    use crossterm::event::Event as Input;
    use yoyomi_core::{Book, Cancel, CellSize, Error, PageKind, Pixels, Result, RgbaImage};
    use yoyomi_render::HalfBlock;

    use super::*;
    use crate::app::tests::{app_with, deliver, keys};
    use crate::app::{App, Event};
    use crate::draw::tests::render;

    /// A book of text pages; the pages in `needles` hold the word needle,
    /// and `broken` is an image page that fails to decode.
    struct TextBook {
        len: usize,
        needles: Vec<usize>,
        broken: Option<usize>,
        path: PathBuf,
    }

    impl Book for TextBook {
        fn path(&self) -> &Path {
            &self.path
        }
        fn title(&self) -> &str {
            "Text"
        }
        fn len(&self) -> usize {
            self.len
        }
        fn kind(&self, page: usize) -> PageKind {
            if self.broken == Some(page) { PageKind::Image } else { PageKind::Text }
        }
        fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
            cancel.check()?;
            if self.broken == Some(page) {
                return Err(Error::Decode("corrupt".into()));
            }
            Ok(Arc::new(RgbaImage::from_pixel(4, 2, [255, 255, 255, 255].into())))
        }
        fn page_text(&self, page: usize) -> Option<Vec<String>> {
            if self.broken == Some(page) {
                return None;
            }
            let word = if self.needles.contains(&page) { "a Needle here" } else { "hay" };
            Some(vec![format!("Page {page}"), word.to_owned()])
        }
    }

    fn reader(needles: &[usize]) -> (App, Receiver<Event>) {
        let (mut app, received) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 80, 24);
        open(&mut app, "/books/text.epub", needles, None);
        (app, received)
    }

    fn open(app: &mut App, path: &str, needles: &[usize], broken: Option<usize>) {
        open_in(app, 0, path, 5, needles, broken);
    }

    fn open_in(app: &mut App, pane: usize, path: &str, len: usize, needles: &[usize], broken: Option<usize>) {
        let book = TextBook { len, needles: needles.to_vec(), broken, path: path.into() };
        deliver(app, pane, path, Ok(Arc::new(book)));
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_event(Event::Input(Input::Key(KeyEvent::from(code))));
    }

    /// Hands the search's steps to `app` until it reports the end.
    fn finish(app: &mut App, received: &Receiver<Event>) {
        while !app.status_left().contains("search done") && !app.status_left().contains("hit ") {
            app.on_event(received.recv_timeout(Duration::from_secs(5)).expect("a search step"));
        }
    }

    fn status(app: &App) -> String {
        app.status_left().rsplit("  ·  ").next().unwrap_or_default().to_owned()
    }

    #[test]
    fn slash_searches_and_n_p_jump_around_the_book() {
        let (mut app, received) = reader(&[1, 3]);
        keys(&mut app, "/needle");
        press(&mut app, KeyCode::Enter);
        assert!(app.search_line.is_none());
        finish(&mut app, &received);
        assert_eq!(status(&app), "hit needle p.2 (2 total)");
        assert!(app.title(0).starts_with("Text  2/5"));
        for (key, page) in [("n", 4), ("n", 2), ("p", 4), ("p", 2)] {
            keys(&mut app, key);
            assert!(app.title(0).starts_with(&format!("Text  {page}/5")), "{key}: {}", app.title(0));
        }
    }

    #[test]
    fn the_final_status_counts_unread_pages() {
        let (mut app, received) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 200, 24);
        open(&mut app, "/books/text.epub", &[1], Some(3));
        keys(&mut app, "/needle");
        press(&mut app, KeyCode::Enter);
        finish(&mut app, &received);
        assert_eq!(status(&app), "hit needle p.2 (1 total) · 1 page(s) not searched");
    }

    #[test]
    fn a_search_without_hits_says_so() {
        let (mut app, received) = reader(&[]);
        keys(&mut app, "n");
        assert_eq!(status(&app), "no search — press / first");
        keys(&mut app, "/needle");
        press(&mut app, KeyCode::Enter);
        finish(&mut app, &received);
        assert_eq!(status(&app), "search done: 0 hit(s) for \"needle\" · n/p to jump");
        keys(&mut app, "n");
        assert_eq!(status(&app), "no hits for \"needle\"");
    }

    #[test]
    fn esc_closes_the_line_and_keys_read_again() {
        let (mut app, _) = reader(&[]);
        keys(&mut app, "/j");
        assert_eq!(app.search_line.as_deref(), Some("j"));
        assert!(app.title(0).starts_with("Text  1/5"), "keys on the line do not turn pages");
        press(&mut app, KeyCode::Esc);
        keys(&mut app, "j");
        assert!(app.search_line.is_none() && app.title(0).starts_with("Text  2/5"));
    }

    #[test]
    fn opening_another_book_drops_the_search() {
        let (mut app, received) = reader(&[0]);
        keys(&mut app, "/needle");
        press(&mut app, KeyCode::Enter);
        let step = received.recv_timeout(Duration::from_secs(5)).expect("a search step");
        open(&mut app, "/books/other.epub", &[0], None);
        app.on_event(step);
        keys(&mut app, "n");
        assert_eq!(status(&app), "no search — press / first");
    }

    #[test]
    fn one_search_serves_both_panes() {
        let (mut app, received) = app_with(Arc::new(HalfBlock::new(CellSize::FALLBACK)), 200, 24);
        keys(&mut app, "v");
        press(&mut app, KeyCode::Esc);
        open_in(&mut app, 0, "/books/a.epub", 5, &[1], None);
        open_in(&mut app, 1, "/books/b.epub", 100, &[70], None);
        // A search of the left book shows its progress whichever pane is active.
        keys(&mut app, "w/needle");
        press(&mut app, KeyCode::Enter);
        keys(&mut app, "w");
        assert!(app.status_left().ends_with("  ·  OCR 0/5 (0 hits)"), "{}", app.status_left());
        // It ends while the right pane is active, so the left pane stays.
        finish(&mut app, &received);
        assert_eq!(status(&app), "search done: 1 hit(s) for \"needle\" · n/p to jump");
        assert!(app.title(0).starts_with("Text  1/5"), "{}", app.title(0));
        // n goes to the search's pane and makes it active.
        keys(&mut app, "n");
        assert_eq!((status(&app), app.active), ("hit needle p.2 (1 total)".to_owned(), 0));
        assert!(app.title(0).starts_with("Text  2/5"), "{}", app.title(0));

        // A search in the right pane replaces the left one's.
        keys(&mut app, "w/needle");
        press(&mut app, KeyCode::Enter);
        finish(&mut app, &received);
        assert_eq!(status(&app), "hit needle p.71 (1 total)");
        keys(&mut app, "wp");
        assert_eq!(app.active, 1, "p goes back to the right pane");
        assert!(app.title(0).starts_with("Text  2/5"), "{}", app.title(0));

        // Opening a book in the other pane cancels the search, and so does closing either pane.
        open_in(&mut app, 0, "/books/c.epub", 5, &[1], None);
        keys(&mut app, "n");
        assert_eq!(status(&app), NO_SEARCH);
        for close in ["x", "wx"] {
            if close == "wx" {
                keys(&mut app, "O");
                press(&mut app, KeyCode::Esc);
                open_in(&mut app, 1, "/books/b.epub", 100, &[70], None);
            }
            keys(&mut app, "w/needle");
            press(&mut app, KeyCode::Enter);
            keys(&mut app, close);
            assert!(!app.status_left().contains("OCR"), "{close}: {}", app.status_left());
            keys(&mut app, "n");
            assert_eq!(status(&app), NO_SEARCH, "{close}");
        }
    }

    #[test]
    fn focus_mode_shows_the_line_in_the_bottom_row() {
        let (mut app, _) = reader(&[]);
        keys(&mut app, "z/needle");
        assert_eq!(app.image_box(0).rows, 23);
        let (buf, _) = render(&app);
        let bottom: String = (0..buf.area.width).map(|x| buf[(x, 23)].symbol()).collect();
        assert_eq!(bottom.trim_end(), format!(" /needle▏{LINE_HINT}"));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.image_box(0).rows, 24);
    }

    #[test]
    fn the_line_edits_runs_and_closes() {
        let mut line = String::new();
        for c in " needle  é".chars() {
            assert_eq!(edit(&mut line, KeyEvent::from(KeyCode::Char(c))), LineKey::Edit);
        }
        assert_eq!(edit(&mut line, KeyEvent::from(KeyCode::Backspace)), LineKey::Edit);
        assert_eq!(line, " needle  ");
        assert_eq!(edit(&mut line, KeyEvent::from(KeyCode::Enter)), LineKey::Run("needle".to_owned()));
        assert_eq!(edit(&mut "  ".to_owned(), KeyEvent::from(KeyCode::Enter)), LineKey::Close, "a blank term searches nothing");
        assert_eq!(edit(&mut line, KeyEvent::from(KeyCode::Esc)), LineKey::Close);
        assert_eq!(edit(&mut line, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), LineKey::Close);
    }
}
