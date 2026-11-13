//! The chapter and bookmark menus: a list over the whole screen with vim keys
//! and a fuzzy filter. The menu tells the app what a key asks for; the app
//! moves the reader and changes the bookmarks.

use std::mem;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use yoyomi_core::{Chapter, current_chapter};
use yoyomi_state::{BookKey, Mark};

use crate::draw::{safe_text, truncate, width};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Chapters,
    Bookmarks,
}

/// One row of a menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The whole text the filter searches; drawing cuts it to the menu's
    /// label column.
    pub label: String,
    /// What follows the label column: the page, and a bookmark's date.
    detail: String,
    pub page: usize,
    /// The book a bookmark is in; chapters belong to the active pane's book.
    pub book: Option<BookKey>,
    /// A bookmark's own name, where renaming starts.
    name: String,
}

/// What a key asks the app to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Stay,
    Close,
    Go(Item),
    Rename(Item, String),
    Delete(Item),
}

/// Where typed keys go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Input {
    #[default]
    List,
    Filter,
    /// The new name of the selected bookmark, as typed so far.
    Rename(String),
}

#[derive(Debug)]
pub struct Menu {
    pub kind: Kind,
    title: String,
    all: Vec<Item>,
    /// The rows the filter keeps, best match first.
    items: Vec<Item>,
    filter: String,
    cursor: usize,
    top: usize,
    loading: bool,
    input: Input,
}

impl Menu {
    fn new(kind: Kind, title: String, all: Vec<Item>, cursor: usize) -> Self {
        let mut menu = Menu { kind, title, all, items: Vec::new(), filter: String::new(), cursor, top: 0, loading: false, input: Input::List };
        menu.apply_filter();
        menu
    }

    /// The chapter menu while the outline of `book_title` loads.
    pub fn loading_chapters(book_title: &str) -> Self {
        Menu { loading: true, ..Self::new(Kind::Chapters, chapters_title(book_title), Vec::new(), 0) }
    }

    /// The outline in its own order, indented by depth, with the chapter
    /// that holds `page` selected and in view on a screen `screen_rows` high.
    pub fn chapters(book_title: &str, chapters: &[Chapter], page: usize, screen_rows: u16) -> Self {
        let items = chapters
            .iter()
            .map(|chapter| Item {
                label: format!("{}{}", "  ".repeat(chapter.depth.min(12)), safe_text(&chapter.title)),
                detail: format!("p.{}", chapter.page + 1),
                page: chapter.page,
                book: None,
                name: String::new(),
            })
            .collect();
        let mut menu = Self::new(Kind::Chapters, chapters_title(book_title), items, current_chapter(chapters, page).unwrap_or(0));
        menu.move_by(0, screen_rows);
        menu
    }

    /// Every bookmark in the store's order: by book, then page.
    pub fn bookmarks(marks: &[Mark]) -> Self {
        let items = marks
            .iter()
            .map(|mark| Item {
                label: safe_text(mark.label()),
                detail: format!("p.{:<5} {}", mark.page + 1, mark.date()),
                page: mark.page,
                book: Some(mark.book.clone()),
                name: mark.name.clone(),
            })
            .collect();
        Self::new(Kind::Bookmarks, "Bookmarks".to_owned(), items, 0)
    }

    /// Takes the rows of `fresh` and keeps the filter and the input. A
    /// bookmark menu keeps its place; a chapter menu takes the place of
    /// `fresh`, the current chapter.
    pub fn refresh(&mut self, fresh: Menu, screen_rows: u16) {
        let (cursor, top) = if self.kind == Kind::Bookmarks { (self.cursor, self.top) } else { (fresh.cursor, fresh.top) };
        *self = Menu { filter: mem::take(&mut self.filter), input: mem::take(&mut self.input), cursor, top, ..fresh };
        self.apply_filter();
        self.move_by(0, screen_rows);
    }

    fn apply_filter(&mut self) {
        if self.filter.is_empty() {
            self.items = self.all.clone();
        } else {
            let mut scored: Vec<(usize, &Item)> = self
                .all
                .iter()
                .filter_map(|item| fuzzy_score(&self.filter, &format!("{} {}", item.label, item.detail)).map(|score| (score, item)))
                .collect();
            scored.sort_by_key(|(score, _)| *score);
            self.items = scored.into_iter().map(|(_, item)| item.clone()).collect();
        }
        self.cursor = self.cursor.min(self.items.len().saturating_sub(1));
        self.top = self.top.min(self.cursor);
    }

    /// Scrolls the list so the cursor shows on a screen `screen_rows` high.
    pub fn fit(&mut self, screen_rows: u16) {
        self.move_by(0, screen_rows);
    }

    /// The row `item` draws: its label cut and padded to the label column,
    /// then its detail.
    fn row(&self, item: &Item) -> String {
        let cols = if self.kind == Kind::Chapters { 40 } else { 36 };
        format!("{} {}", pad(truncate(&item.label, cols), usize::from(cols)), item.detail)
    }

    fn move_by(&mut self, delta: isize, screen_rows: u16) {
        if self.items.is_empty() {
            return;
        }
        let rows = list_rows(screen_rows);
        self.cursor = self.cursor.saturating_add_signed(delta).min(self.items.len() - 1);
        self.top = self.top.min(self.cursor).max((self.cursor + 1).saturating_sub(rows));
    }

    fn selected(&self) -> Option<Item> {
        self.items.get(self.cursor).cloned()
    }

    /// Handles a key by its name (`j`, `down`, `esc` and the like), on a
    /// screen `screen_rows` high.
    pub fn key(&mut self, key: &str, screen_rows: u16) -> Action {
        match &mut self.input {
            Input::Filter => match key {
                "esc" => {
                    self.filter.clear();
                    self.input = Input::List;
                    self.apply_filter();
                }
                "enter" => self.input = Input::List,
                "backspace" => {
                    self.filter.pop();
                    self.apply_filter();
                }
                _ => {
                    if let Some(c) = typed(key) {
                        self.filter.push(c);
                        (self.cursor, self.top) = (0, 0);
                        self.apply_filter();
                    }
                }
            },
            Input::Rename(name) => match key {
                "esc" => self.input = Input::List,
                "enter" => {
                    let name = mem::take(name);
                    self.input = Input::List;
                    return self.selected().map_or(Action::Stay, |item| Action::Rename(item, name));
                }
                "backspace" => {
                    name.pop();
                }
                _ => name.extend(typed(key)),
            },
            Input::List => match key {
                "esc" if !self.filter.is_empty() => {
                    self.filter.clear();
                    self.apply_filter();
                }
                "esc" | "q" | "tab" | "F" => return Action::Close,
                "j" | "down" => self.move_by(1, screen_rows),
                "k" | "up" => self.move_by(-1, screen_rows),
                "g" => self.move_by(isize::MIN, screen_rows),
                "G" => self.move_by(isize::MAX, screen_rows),
                "/" => self.input = Input::Filter,
                "r" if self.kind == Kind::Bookmarks => {
                    if let Some(item) = self.selected() {
                        self.input = Input::Rename(item.name);
                    }
                }
                "d" if self.kind == Kind::Bookmarks => return self.selected().map_or(Action::Stay, Action::Delete),
                "enter" | "l" => return self.selected().map_or(Action::Close, Action::Go),
                _ => {}
            },
        }
        Action::Stay
    }

    /// Draws the menu over `area`: the title, the rows that fit, a blank row
    /// and the footer.
    pub fn draw(&self, buf: &mut Buffer, area: Rect) {
        let (bold, dim) = (Style::new().add_modifier(Modifier::BOLD).fg(Color::Indexed(212)), Style::new().add_modifier(Modifier::DIM));
        let cols = usize::from(area.width);
        let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
        let mut head = vec![(format!(" {}", self.title), bold)];
        if !self.filter.is_empty() {
            head.push((format!("  ({}/{})", self.items.len(), self.all.len()), dim));
        }
        lines.push(head);
        if self.loading {
            lines.push(vec![("  loading chapters…".to_owned(), dim)]);
        } else if self.items.is_empty() {
            lines.push(vec![("  (no matches)".to_owned(), dim)]);
        }
        for (i, item) in self.items.iter().enumerate().skip(self.top).take(list_rows(area.height)) {
            let line = truncate(&format!("  {}", self.row(item)), area.width.saturating_sub(2));
            lines.push(if i == self.cursor {
                vec![(format!("> {}", line.strip_prefix("  ").unwrap_or(&line)), bold)]
            } else {
                vec![(line, Style::new())]
            });
        }
        lines.push(Vec::new());
        lines.push(match &self.input {
            Input::Filter => {
                vec![(format!(" /{}▏", safe_text(&self.filter)), bold), ("  fuzzy filter · enter keep · esc clear".to_owned(), dim)]
            }
            Input::Rename(name) => {
                vec![(format!(" rename: {}▏", safe_text(name)), bold), ("  enter save · empty reverts to title · esc cancel".to_owned(), dim)]
            }
            Input::List if self.kind == Kind::Bookmarks => {
                vec![(" j/k move · enter go · / filter · r rename · d delete · esc close".to_owned(), dim)]
            }
            Input::List => vec![(" j/k move · enter go · / filter · esc close".to_owned(), dim)],
        });
        for (y, line) in (area.top()..area.bottom()).zip(lines) {
            let mut x = area.left();
            for (text, style) in line {
                let room = cols.saturating_sub(usize::from(x - area.left()));
                x = buf.set_stringn(x, y, text, room, style).0;
            }
        }
    }
}

/// The rows of items a menu shows on a screen `screen_rows` high.
fn list_rows(screen_rows: u16) -> usize {
    usize::from(screen_rows.saturating_sub(3)).max(1)
}

fn chapters_title(book_title: &str) -> String {
    format!("Chapters — {}", safe_text(book_title))
}

/// The character a key types, when it types one.
fn typed(key: &str) -> Option<char> {
    let mut chars = key.chars();
    chars.next().filter(|_| chars.next().is_none())
}

/// `text` padded with spaces to `cols` cells.
fn pad(text: String, cols: usize) -> String {
    let gap = cols.saturating_sub(width(&text));
    text + &" ".repeat(gap)
}

/// How well `query` matches `text` as a case-insensitive subsequence: lower
/// is better, compact matches first, then early ones. `None` when it does not
/// match.
fn fuzzy_score(query: &str, text: &str) -> Option<usize> {
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let (mut first, mut last, mut matched) = (None, 0, 0);
    for (i, c) in text.to_lowercase().chars().enumerate() {
        if matched < query.len() && c == query[matched] {
            first = first.or(Some(i));
            last = i;
            matched += 1;
        }
    }
    if matched < query.len() {
        return None;
    }
    let Some(first) = first else { return Some(0) };
    Some((last - first + 1 - query.len()) * 4 + first)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn chapter(title: &str, page: usize, depth: usize) -> Chapter {
        Chapter { title: title.into(), page, depth }
    }

    fn labels(menu: &Menu) -> Vec<String> {
        menu.items.iter().map(|item| menu.row(item).trim_end().to_owned()).collect()
    }

    fn rows(menu: &Menu, cols: u16, screen_rows: u16) -> Vec<String> {
        let area = Rect::new(0, 0, cols, screen_rows);
        let mut buf = Buffer::empty(area);
        menu.draw(&mut buf, area);
        (0..screen_rows).map(|y| (0..cols).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_owned()).collect()
    }

    #[test]
    fn fuzzy_scores_rank_compact_then_early_matches() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        assert_eq!(fuzzy_score("LAST", "the last"), Some(4));
        assert_eq!(fuzzy_score("lt", "last"), Some(8));
        assert_eq!(fuzzy_score("ab", "ba"), None);
        let items =
            ["alpha beta", "abc", "xab"].map(|label| Item { label: label.into(), detail: String::new(), page: 0, book: None, name: String::new() });
        let mut menu = Menu::new(Kind::Chapters, String::new(), items.to_vec(), 0);
        for c in "ab".chars() {
            menu.key("/", 24);
            menu.key(&c.to_string(), 24);
            menu.key("enter", 24);
        }
        assert_eq!(labels(&menu), ["abc", "xab", "alpha beta"]);
    }

    #[test]
    fn chapters_keep_outline_order_and_indent_by_depth() {
        let outline = [chapter("Later", 2, 0), chapter("Earlier", 0, 0), chapter("Nested\x1b[31m title", 1, 2), chapter("Appendix", 0, 0)];
        let menu = Menu::chapters("Outline", &outline, 1, 24);
        assert_eq!(menu.cursor, 2);
        assert_eq!(menu.items[0].page, 2);
        let rows = labels(&menu);
        assert!(rows[2].starts_with("    Nested title ") && rows[2].ends_with(" p.2"), "{:?}", rows[2]);
        assert_eq!(width(&menu.row(&menu.items[0])), 40 + " p.3".len());
    }

    #[test]
    fn keys_move_filter_and_go() {
        let outline: Vec<Chapter> = (0..30).map(|i| chapter(&format!("Part {i}"), i, 0)).collect();
        let mut menu = Menu::chapters("Book", &outline, 29, 10);
        assert_eq!((menu.cursor, menu.top), (29, 23), "the current chapter opens in view");
        menu.key("g", 10);
        assert_eq!(menu.key("G", 10), Action::Stay);
        assert_eq!((menu.cursor, menu.top), (29, 23), "the cursor stays on screen");
        menu.key("g", 10);
        menu.key("j", 10);
        assert_eq!((menu.cursor, menu.top), (1, 0));
        for key in ["/", "2", "9"] {
            menu.key(key, 10);
        }
        assert_eq!(menu.items.iter().map(|item| item.page).collect::<Vec<_>>(), [29, 28], "the compact match first");
        assert_eq!(menu.key("esc", 10), Action::Stay, "esc in the filter clears it");
        assert_eq!(menu.items.len(), 30);
        for key in ["/", "7", "enter"] {
            menu.key(key, 10);
        }
        let Action::Go(item) = menu.key("l", 10) else { panic!("l goes to the chapter") };
        assert_eq!(item.page, 7);
        assert_eq!(menu.key("esc", 10), Action::Stay, "esc clears a kept filter first");
        assert_eq!(menu.key("esc", 10), Action::Close);
        for key in ["q", "tab", "F"] {
            assert_eq!(menu.key(key, 10), Action::Close);
        }
        assert_eq!(menu.key("d", 10), Action::Stay, "chapters cannot be deleted");
    }

    fn mark(title: &str, name: &str, page: usize) -> Mark {
        Mark {
            book: BookKey::new(Path::new(&format!("/books/{title}.cbz"))),
            title: title.into(),
            name: name.into(),
            page,
            added: "2023-11-14T00:30:00+09:00".into(),
        }
    }

    #[test]
    fn bookmarks_rename_and_delete_the_selected_mark() {
        let mut menu = Menu::bookmarks(&[mark("Akira", "", 4), mark("Berserk", "eclipse", 82)]);
        assert_eq!(labels(&menu)[0], format!("Akira{} p.5     2023-11-14", " ".repeat(31)));
        assert!(labels(&menu)[1].starts_with("eclipse "));
        menu.key("j", 24);
        menu.key("r", 24);
        for key in ["backspace", "!", " ", "x"] {
            menu.key(key, 24);
        }
        let Action::Rename(item, name) = menu.key("enter", 24) else { panic!("enter saves the name") };
        assert_eq!((item.page, item.book.as_ref().map(BookKey::path), name.as_str()), (82, Some(Path::new("/books/Berserk.cbz")), "eclips! x"));
        menu.key("r", 24);
        assert_eq!(menu.key("esc", 24), Action::Stay);
        assert_eq!(menu.input, Input::List, "esc leaves the rename");
        let Action::Delete(item) = menu.key("d", 24) else { panic!("d deletes") };
        assert_eq!(item.page, 82);

        menu.refresh(Menu::bookmarks(&[mark("Akira", "", 4)]), 24);
        assert_eq!((menu.cursor, menu.items.len()), (0, 1), "the place stays in range");
    }

    #[test]
    fn menus_draw_title_rows_and_footer() {
        let outline = [chapter("One", 0, 0), chapter("Two", 1, 1)];
        let mut menu = Menu::chapters("Book\x1b]2;x\x07", &outline, 1, 8);
        let screen = rows(&menu, 60, 8);
        assert_eq!(screen[0], " Chapters — Book");
        assert_eq!(screen[1], format!("  One{} p.1", " ".repeat(37)));
        assert_eq!(screen[2], format!(">   Two{} p.2", " ".repeat(35)));
        assert_eq!(screen[3], "");
        assert_eq!(screen[4], " j/k move · enter go · / filter · esc close");
        menu.key("/", 8);
        menu.key("z", 8);
        let screen = rows(&menu, 60, 8);
        assert_eq!(screen[0], " Chapters — Book  (0/2)");
        assert_eq!(screen[1], "  (no matches)");
        assert_eq!(screen[3], " /z▏  fuzzy filter · enter keep · esc clear");
        assert_eq!(rows(&Menu::loading_chapters("Book"), 60, 8)[1], "  loading chapters…");
        assert_eq!(rows(&menu, 3, 1).len(), 1, "a tiny screen draws what fits");
    }

    #[test]
    fn filters_match_words_past_the_cut() {
        let long = format!("{} Epilogue", "Chapter ".repeat(6));
        let outline = [chapter(&long, 0, 0), chapter("Short", 1, 0)];
        let mut menu = Menu::chapters("Book", &outline, 0, 24);
        for key in ["/", "e", "p", "i", "l"] {
            menu.key(key, 24);
        }
        assert_eq!(menu.items.iter().map(|item| item.page).collect::<Vec<_>>(), [0]);
        assert!(!labels(&menu)[0].contains("Epilogue"), "the row still cuts at 40 cells: {:?}", labels(&menu)[0]);
        assert_eq!(width(&menu.row(&menu.items[0])), 40 + " p.1".len());

        let mut menu = Menu::bookmarks(&[mark("Akira", &format!("{} ending", "long ".repeat(8)), 4), mark("Berserk", "", 82)]);
        for key in ["/", "e", "n", "d", "i", "n", "g"] {
            menu.key(key, 24);
        }
        assert_eq!(menu.items.iter().map(|item| item.page).collect::<Vec<_>>(), [4]);
        assert!(!labels(&menu)[0].contains("ending"));
    }

    #[test]
    fn a_shorter_screen_keeps_the_cursor_in_view() {
        let outline: Vec<Chapter> = (0..30).map(|i| chapter(&format!("Part {i}"), i, 0)).collect();
        let mut menu = Menu::chapters("Book", &outline, 29, 24);
        assert_eq!((menu.cursor, menu.top), (29, 9));
        menu.fit(10);
        assert_eq!((menu.cursor, menu.top), (29, 23));
        assert!(rows(&menu, 60, 10)[7].starts_with("> Part 29"), "{:?}", rows(&menu, 60, 10));
    }
}
