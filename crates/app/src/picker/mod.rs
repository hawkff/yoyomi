//! The file picker behind `o` and `O`: folders and the books yoyomi reads,
//! with vim and arrow keys.

use std::fs;
use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::draw::{safe_text, truncate};

/// The extensions of the books yoyomi opens; the picker matches them in any case.
const BOOK_EXTENSIONS: &[&str] = &["cbz", "zip", "cbr", "rar", "epub", "fb2", "docx", "doc", "pdf", "djvu", "djv"];

/// What a key asks the app to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Picked {
    Stay,
    Cancel,
    Book(PathBuf),
}

struct Entry {
    path: PathBuf,
    /// The file name as the screen shows it.
    name: String,
    dir: bool,
}

pub struct Picker {
    dir: PathBuf,
    /// Folders first, then books, each by name; hidden files stay out.
    entries: Vec<Entry>,
    /// Why the folder could not be read.
    error: Option<String>,
    cursor: usize,
    top: usize,
}

impl Picker {
    /// A picker that starts in `dir`; `show` reads it.
    pub fn new(dir: PathBuf) -> Self {
        Picker { dir, entries: Vec::new(), error: None, cursor: 0, top: 0 }
    }

    /// Reads the folder again, keeping the place in it.
    pub fn show(&mut self) {
        let selected = self.entries.get(self.cursor).map(|entry| entry.path.clone());
        self.read();
        self.select(selected.as_deref());
    }

    fn read(&mut self) {
        self.error = None;
        self.entries = match fs::read_dir(&self.dir) {
            Ok(listing) => listing
                .flatten()
                .filter_map(|item| {
                    let name = item.file_name().to_string_lossy().into_owned();
                    let path = item.path();
                    // metadata follows links, so a link to a folder opens as one.
                    let dir = fs::metadata(&path).is_ok_and(|meta| meta.is_dir());
                    (!name.starts_with('.') && (dir || is_book(&path))).then_some(Entry { path, name: safe_text(&name), dir })
                })
                .collect(),
            Err(error) => {
                self.error = Some(format!("{}: {error}", self.dir.display()));
                Vec::new()
            }
        };
        self.entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
    }

    /// Puts the cursor on `path`, or on the first entry.
    fn select(&mut self, path: Option<&Path>) {
        self.cursor = path.and_then(|path| self.entries.iter().position(|entry| entry.path == path)).unwrap_or(0);
        self.top = self.cursor;
    }

    fn enter(&mut self, dir: PathBuf) {
        let from = std::mem::replace(&mut self.dir, dir);
        self.read();
        self.select(Some(&from));
    }

    /// Scrolls the list so the cursor shows on a screen `screen_rows` high.
    pub fn fit(&mut self, screen_rows: u16) {
        self.move_by(0, screen_rows);
    }

    fn move_by(&mut self, delta: isize, screen_rows: u16) {
        let rows = list_rows(screen_rows);
        self.cursor = self.cursor.saturating_add_signed(delta).min(self.entries.len().saturating_sub(1));
        self.top = self.top.min(self.cursor).max((self.cursor + 1).saturating_sub(rows));
    }

    /// Handles a key by its name (`j`, `pgdown`, `ctrl+n` and the like), on a
    /// screen `screen_rows` high.
    pub fn key(&mut self, key: &str, screen_rows: u16) -> Picked {
        let page = list_rows(screen_rows) as isize;
        match key {
            "esc" | "ctrl+c" => return Picked::Cancel,
            "j" | "down" | "ctrl+n" => self.move_by(1, screen_rows),
            "k" | "up" | "ctrl+p" => self.move_by(-1, screen_rows),
            "J" | "pgdown" => self.move_by(page, screen_rows),
            "K" | "pgup" => self.move_by(-page, screen_rows),
            "g" => self.move_by(isize::MIN, screen_rows),
            "G" => self.move_by(isize::MAX, screen_rows),
            "h" | "left" | "backspace" => {
                if let Some(parent) = self.dir.parent() {
                    self.enter(parent.to_owned());
                }
            }
            "l" | "right" | "enter" => match self.entries.get(self.cursor).map(|entry| (entry.path.clone(), entry.dir)) {
                Some((path, true)) => self.enter(path),
                Some((path, false)) if key == "enter" => return Picked::Book(path),
                _ => {}
            },
            _ => {}
        }
        Picked::Stay
    }

    /// Draws the picker over `area` for pane `pane` (0 or 1).
    pub fn draw(&self, buf: &mut Buffer, area: Rect, pane: usize) {
        let bold = Style::new().add_modifier(Modifier::BOLD).fg(Color::Indexed(212));
        let dim = Style::new().add_modifier(Modifier::DIM);
        let cols = usize::from(area.width);
        let (x, bottom) = (area.left(), area.bottom());
        buf.set_stringn(x, area.top(), format!(" Open book → pane {} ", pane + 1), cols, bold);
        let list = area.top() + 2;
        let rows = list_rows(area.height);
        let empty = self.error.clone().or_else(|| self.entries.is_empty().then(|| "no books here".to_owned()));
        if let Some(message) = empty
            && list < bottom
        {
            buf.set_stringn(x, list, format!("  {}", safe_text(&message)), cols, dim);
        }
        for (y, (i, entry)) in (list..bottom).zip(self.entries.iter().enumerate().skip(self.top).take(rows)) {
            let name = if entry.dir { format!("{}/", entry.name) } else { entry.name.clone() };
            let (mark, style) =
                if i == self.cursor { ("> ", bold) } else { ("  ", if entry.dir { Style::new().fg(Color::Indexed(99)) } else { Style::new() }) };
            buf.set_stringn(x, y, truncate(&format!("{mark}{name}"), area.width), cols, style);
        }
        if area.height > 1 {
            buf.set_stringn(x, bottom - 1, " enter select · h/l dirs · esc cancel", cols, dim);
        }
    }
}

/// The rows of entries the picker shows on a screen `screen_rows` high.
fn list_rows(screen_rows: u16) -> usize {
    usize::from(screen_rows.saturating_sub(4)).max(1)
}

fn is_book(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| BOOK_EXTENSIONS.iter().any(|book| ext.eq_ignore_ascii_case(book)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(picker: &Picker) -> Vec<&str> {
        picker.entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[test]
    fn lists_folders_then_books_in_any_case() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.CBZ", "a.pdf", "notes.txt", ".hidden.cbz", "c.Djvu", "noext", "d.EPUB", "e.fb2", "f.Docx", "g.DOC"] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        fs::create_dir(dir.path().join("zines")).unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let mut picker = Picker::new(dir.path().to_owned());
        picker.show();
        assert_eq!(names(&picker), ["zines", "a.pdf", "b.CBZ", "c.Djvu", "d.EPUB", "e.fb2", "f.Docx", "g.DOC"]);
    }

    #[test]
    fn keys_walk_folders_and_pick_a_book() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("inner")).unwrap();
        fs::write(dir.path().join("inner/deep.cbr"), b"").unwrap();
        fs::write(dir.path().join("top.cbz"), b"").unwrap();
        let mut picker = Picker::new(dir.path().to_owned());
        picker.show();
        assert_eq!(picker.key("l", 24), Picked::Stay);
        assert_eq!(names(&picker), ["deep.cbr"]);
        assert_eq!(picker.key("l", 24), Picked::Stay, "l does not pick a book");
        assert_eq!(picker.key("enter", 24), Picked::Book(dir.path().join("inner/deep.cbr")));
        picker.key("h", 24);
        assert_eq!(picker.entries[picker.cursor].name, "inner", "back to the folder it left");
        picker.key("G", 24);
        assert_eq!(picker.key("enter", 24), Picked::Book(dir.path().join("top.cbz")));
        assert_eq!(picker.key("esc", 24), Picked::Cancel);
    }

    #[test]
    fn a_shorter_screen_keeps_the_cursor_in_view() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..30 {
            fs::write(dir.path().join(format!("{i:02}.cbz")), b"").unwrap();
        }
        let mut picker = Picker::new(dir.path().to_owned());
        picker.show();
        picker.key("G", 24);
        assert_eq!((picker.cursor, picker.top), (29, 10));
        picker.fit(10);
        assert_eq!((picker.cursor, picker.top), (29, 24), "six rows show 24 to 29");
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        picker.draw(&mut buf, area, 0);
        assert_eq!((0..40).map(|x| buf[(x, 7)].symbol()).collect::<String>().trim_end(), "> 29.cbz");
    }

    #[test]
    fn draws_the_header_entries_and_hint() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("inner")).unwrap();
        fs::write(dir.path().join("book.cbz"), b"").unwrap();
        let mut picker = Picker::new(dir.path().to_owned());
        picker.show();
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        picker.draw(&mut buf, area, 1);
        let row = |y: u16| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_owned();
        assert_eq!(row(0), " Open book → pane 2");
        assert_eq!(row(2), "> inner/");
        assert_eq!(row(3), "  book.cbz");
        assert_eq!(row(7), " enter select · h/l dirs · esc cancel");

        let mut gone = Picker::new(dir.path().join("missing"));
        gone.show();
        assert!(gone.error.as_deref().is_some_and(|error| error.contains("missing")));
        assert_eq!(gone.key("enter", 24), Picked::Stay);
    }
}
