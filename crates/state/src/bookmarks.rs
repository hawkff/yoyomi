//! Bookmarks on pages of any book.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use yoyomi_core::{Error, Result};

use crate::storage::{self, BookKey};
use crate::time;

/// One bookmarked page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    /// The key of the marked book.
    pub book: BookKey,
    pub title: String,
    /// A label the reader gave; empty means the title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Zero-based page index.
    pub page: usize,
    /// When the mark was added, as RFC 3339 text with the UTC offset it was
    /// taken in. Integer seconds since the Unix epoch load as that moment at
    /// the local offset.
    #[serde(deserialize_with = "deserialize_added")]
    pub added: String,
}

impl Mark {
    /// The label menus show: the reader's name for the mark, else the title.
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.title } else { &self.name }
    }

    /// The date of `added` at its own UTC offset, as YYYY-MM-DD.
    pub fn date(&self) -> &str {
        self.added.get(..10).unwrap_or(&self.added)
    }
}

fn deserialize_added<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Added {
        Text(String),
        Seconds(i64),
    }
    match Added::deserialize(deserializer)? {
        Added::Text(text) if time::is_rfc3339(&text) => Ok(text),
        Added::Text(text) => Err(D::Error::custom(format!("not an RFC 3339 time: {text:?}"))),
        Added::Seconds(secs) => Ok(time::local(secs)),
    }
}

/// All bookmarks, sorted by book and page.
#[derive(Debug, Default)]
pub struct Bookmarks {
    path: Option<PathBuf>,
    pub marks: Vec<Mark>,
}

/// The contents of `bookmarks.json`.
#[derive(Default, Serialize, Deserialize)]
struct BookmarksFile {
    /// `null` reads as no marks.
    #[serde(deserialize_with = "deserialize_marks")]
    marks: Vec<Mark>,
}

fn deserialize_marks<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Vec<Mark>, D::Error> {
    Option::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn read(path: &Path) -> Vec<Mark> {
    fs::read(path).ok().and_then(|data| serde_json::from_slice::<BookmarksFile>(&data).ok()).unwrap_or_default().marks
}

fn sort(marks: &mut [Mark]) {
    marks.sort_by(|a, b| (&a.book, a.page).cmp(&(&b.book, b.page)));
}

impl Bookmarks {
    /// Reads `bookmarks.json` from the config directory. A missing file
    /// gives an empty store.
    pub fn load() -> Self {
        Self::load_from(crate::config_file("bookmarks.json"))
    }

    /// Reads bookmarks from `path`; `None` keeps them in memory only, and
    /// changes then fail.
    pub fn load_from(path: Option<PathBuf>) -> Self {
        let marks = path.as_deref().map(read).unwrap_or_default();
        Bookmarks { path, marks }
    }

    /// Adds a mark on `page` of `book`, or removes the one there. Returns
    /// whether it added one.
    pub fn toggle(&mut self, book: &BookKey, title: &str, page: usize) -> Result<bool> {
        self.update(|marks| {
            if let Some(index) = find(marks, book, page) {
                marks.remove(index);
                return false;
            }
            marks.push(Mark { book: book.clone(), title: title.to_owned(), name: String::new(), page, added: time::now() });
            true
        })
    }

    /// Whether `page` of `book` has a mark.
    pub fn has(&self, book: &BookKey, page: usize) -> bool {
        find(&self.marks, book, page).is_some()
    }

    /// Sets the label of a mark; an empty name brings back the title.
    pub fn rename(&mut self, book: &BookKey, page: usize, name: &str) -> Result<()> {
        self.update(|marks| {
            if let Some(index) = find(marks, book, page) {
                marks[index].name = name.trim().to_owned();
            }
        })
    }

    /// Deletes the mark on `page` of `book`.
    pub fn remove(&mut self, book: &BookKey, page: usize) -> Result<()> {
        self.update(|marks| {
            if let Some(index) = find(marks, book, page) {
                marks.remove(index);
            }
        })
    }

    /// Applies `change` to the marks in the file as it is now and writes them
    /// back. The store takes the result only once the file is written.
    fn update<T>(&mut self, change: impl FnOnce(&mut Vec<Mark>) -> T) -> Result<T> {
        let path =
            self.path.as_deref().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the config directory for bookmarks is unavailable"))?;
        let mut latest = BookmarksFile::default();
        let mut result = None;
        storage::update(path, |data| {
            if !data.is_empty() {
                latest = serde_json::from_slice(data).map_err(|error| Error::Format(format!("{}: {error}", path.display())))?;
            }
            result = Some(change(&mut latest.marks));
            sort(&mut latest.marks);
            serde_json::to_vec_pretty(&latest).map_err(|error| Error::Format(error.to_string()))
        })?;
        self.marks = latest.marks;
        Ok(result.expect("storage::update calls apply before it succeeds"))
    }
}

/// The mark on `page` of the book with key `book`.
fn find(marks: &[Mark], book: &BookKey, page: usize) -> Option<usize> {
    marks.iter().position(|mark| mark.book == *book && mark.page == page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_preserve_other_changes_and_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookmarks.json");
        let (mut a, mut b) = (Bookmarks::load_from(Some(path.clone())), Bookmarks::load_from(Some(path.clone())));
        let (book_a, book_b) = (&BookKey::new(Path::new("/a")), &BookKey::new(Path::new("/b")));
        assert!(a.toggle(book_a, "A", 1).unwrap());
        assert!(b.toggle(book_b, "B", 2).unwrap());
        assert!(b.has(book_a, 1), "second writer lost first mark");
        a.remove(book_a, 1).unwrap();
        b.rename(book_b, 2, " new ").unwrap();

        let saved: BookmarksFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.marks.len(), 1, "saved marks: {:?}", saved.marks);
        assert_eq!((&saved.marks[0].book, saved.marks[0].name.as_str()), (book_b, "new"));
        assert_eq!(b.marks, saved.marks);

        b.rename(book_b, 2, "  ").unwrap();
        assert_eq!(b.marks[0].label(), "B");
        assert!(!b.toggle(book_b, "B", 2).unwrap());
        assert!(Bookmarks::load_from(Some(path)).marks.is_empty());
    }

    #[test]
    fn marks_stay_sorted_by_book_then_page() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Bookmarks::load_from(Some(dir.path().join("bookmarks.json")));
        for (book, page) in [("/b", 3), ("/a", 9), ("/b", 1), ("/a", 2)] {
            store.toggle(&BookKey::new(Path::new(book)), "T", page).unwrap();
        }
        let order: Vec<_> = store.marks.iter().map(|mark| (mark.book.path().to_str().unwrap(), mark.page)).collect();
        assert_eq!(order, [("/a", 2), ("/a", 9), ("/b", 1), ("/b", 3)]);
        assert!(store.marks.iter().all(|mark| time::is_rfc3339(&mark.added)));
    }

    #[test]
    fn write_failure_preserves_memory() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_directory = dir.path().join("not-a-directory");
        fs::write(&not_a_directory, "").unwrap();
        let mut store = Bookmarks::load_from(Some(not_a_directory.join("bookmarks.json")));
        let kept = Mark {
            book: BookKey::new(Path::new("/kept")),
            title: "K".into(),
            name: String::new(),
            page: 0,
            added: "2026-11-12T09:30:00+09:00".into(),
        };
        store.marks.push(kept.clone());
        assert!(store.toggle(&BookKey::new(Path::new("/a")), "A", 0).is_err());
        assert_eq!(store.marks, [kept]);
    }

    #[test]
    fn marks_use_the_position_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Bookmarks::load_from(Some(dir.path().join("bookmarks.json")));
        let key = |book: &str| BookKey::new(Path::new(book));
        assert!(store.toggle(&key("/books/sub/../a.cbz"), "A", 0).unwrap());
        assert_eq!(store.marks[0].book, key("/books/a.cbz"));
        assert!(store.has(&key("/books/./a.cbz"), 0));
        store.rename(&key("/books/a.cbz"), 0, "named").unwrap();
        assert_eq!(store.marks[0].name, "named");
        assert!(!store.toggle(&key("/books/a.cbz"), "A", 0).unwrap(), "second spelling added a second mark");

        assert!(store.toggle(&key("relative.cbz"), "R", 0).unwrap());
        assert!(store.marks[0].book.path().is_absolute());
        store.remove(&BookKey::new(&std::env::current_dir().unwrap().join("relative.cbz")), 0).unwrap();
        assert!(store.marks.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn names_that_are_not_utf8_keep_separate_marks() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookmarks.json");
        let ff = dir.path().join(std::ffi::OsStr::from_bytes(b"\xffodd.cbz"));
        let fe = dir.path().join(std::ffi::OsStr::from_bytes(b"\xfeodd.cbz"));
        let (ff, fe) = (BookKey::new(&ff), BookKey::new(&fe));
        let mut store = Bookmarks::load_from(Some(path.clone()));
        assert!(store.toggle(&ff, "FF", 0).unwrap());
        assert!(store.toggle(&fe, "FE", 0).unwrap(), "0xfe name found the 0xff mark");
        assert!(store.has(&ff, 0) && store.has(&fe, 0));

        let store = Bookmarks::load_from(Some(path.clone()));
        let books: Vec<_> = store.marks.iter().map(|mark| (mark.book.clone(), mark.title.as_str())).collect();
        assert_eq!(books, [(fe.clone(), "FE"), (ff.clone(), "FF")]);
        let mut store = Bookmarks::load_from(Some(path.clone()));
        store.remove(&fe, 0).unwrap();
        let store = Bookmarks::load_from(Some(path));
        assert!(store.has(&ff, 0) && !store.has(&fe, 0));
    }

    /// A `bookmarks.json` indented by two spaces, with an empty name left out
    /// and times in nanoseconds at an offset or in whole seconds at `Z`.
    const BOOKMARKS_FILE: &str = r#"{
  "marks": [
    {
      "book": "/books/a.cbz",
      "title": "A",
      "page": 3,
      "added": "2026-11-12T00:30:00.123456789+09:00"
    },
    {
      "book": "/books/b.cbz",
      "title": "B",
      "name": "fight",
      "page": 0,
      "added": "2026-11-11T23:59:59Z"
    }
  ]
}"#;

    #[test]
    fn bookmark_files_load_and_keep_their_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookmarks.json");
        fs::write(&path, BOOKMARKS_FILE).unwrap();
        let mut store = Bookmarks::load_from(Some(path.clone()));
        let rows: Vec<_> = store.marks.iter().map(|mark| (mark.label(), mark.page, mark.date())).collect();
        assert_eq!(rows, [("A", 3, "2026-11-12"), ("fight", 0, "2026-11-11")]);
        store.rename(&BookKey::new(Path::new("/books/a.cbz")), 3, "named").unwrap();
        assert!(store.toggle(&BookKey::new(Path::new("/books/c.cbz")), "C", 1).unwrap());

        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let marks = saved["marks"].as_array().unwrap();
        let first =
            serde_json::json!({"book": "/books/a.cbz", "title": "A", "name": "named", "page": 3, "added": "2026-11-12T00:30:00.123456789+09:00"});
        assert_eq!(marks[0], first);
        assert_eq!(marks[1]["added"], "2026-11-11T23:59:59Z");
        assert!(time::is_rfc3339(marks[2]["added"].as_str().unwrap()), "{}", marks[2]);
    }

    #[test]
    fn null_marks_and_integer_times_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookmarks.json");
        fs::write(&path, r#"{"marks": null}"#).unwrap();
        let mut store = Bookmarks::load_from(Some(path.clone()));
        assert!(store.marks.is_empty());
        assert!(store.toggle(&BookKey::new(Path::new("/a")), "A", 0).unwrap(), "a change over null marks failed");

        fs::write(&path, r#"{"marks": [{"book": "/a", "title": "A", "page": 0, "added": 1700000000}]}"#).unwrap();
        assert_eq!(Bookmarks::load_from(Some(path.clone())).marks[0].added, time::local(1_700_000_000));
        fs::write(&path, r#"{"marks": [{"book": "/a", "title": "A", "page": 0, "added": "yesterday"}]}"#).unwrap();
        assert!(Bookmarks::load_from(Some(path)).marks.is_empty());
    }

    #[test]
    fn changes_without_a_path_fail() {
        let mut store = Bookmarks::load_from(None);
        let error = store.toggle(&BookKey::new(Path::new("/a")), "A", 0).unwrap_err();
        assert!(error.to_string().contains("config directory"), "{error}");
        assert!(store.marks.is_empty());
    }
}
