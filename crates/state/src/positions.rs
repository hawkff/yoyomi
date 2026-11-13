//! The last position in each book.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use yoyomi_core::{Error, Result};

use crate::storage::{self, BookKey};

/// The last place a reader saw in a book.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Zero-based page index.
    pub page: usize,
    /// In webtoon mode, the fraction of `page` above the top of the viewport.
    #[serde(default)]
    pub offset: f64,
    /// Webtoon scrolling still pending, in terminal rows.
    #[serde(default)]
    pub scroll: f64,
    #[serde(default)]
    pub webtoon: bool,
}

/// The contents of `progress.json`.
#[derive(Default, Serialize, Deserialize)]
struct ProgressFile {
    positions: BTreeMap<String, Position>,
}

/// Saved positions, keyed by absolute book path.
#[derive(Debug, Default)]
pub struct Positions {
    path: Option<PathBuf>,
    /// Keyed by the text of the book's key.
    positions: BTreeMap<String, Position>,
    /// Changes since the last save; `None` marks a deletion.
    pending: BTreeMap<String, Option<Position>>,
}

fn read(path: &Path) -> BTreeMap<String, Position> {
    fs::read(path).ok().and_then(|data| serde_json::from_slice::<ProgressFile>(&data).ok()).unwrap_or_default().positions
}

impl Positions {
    /// Reads `progress.json` from the config directory. Missing or broken
    /// data gives an empty store.
    pub fn load() -> Self {
        Self::load_from(crate::config_file("progress.json"))
    }

    /// Reads positions from `path`; `None` keeps them in memory only.
    pub fn load_from(path: Option<PathBuf>) -> Self {
        let positions = path.as_deref().map(read).unwrap_or_default();
        Positions { path, positions, pending: BTreeMap::new() }
    }

    /// The saved position of `book`.
    pub fn get(&self, book: &BookKey) -> Option<Position> {
        self.positions.get(&book.text()).copied()
    }

    /// Changes the position of `book` in memory; `save` writes it.
    pub fn set(&mut self, book: &BookKey, mut position: Position) {
        position.offset = if position.offset.is_nan() { 0.0 } else { position.offset.clamp(0.0, 1.0) };
        // JSON has no NaN or infinity, and one such number would break the whole file.
        if !position.scroll.is_finite() {
            position.scroll = 0.0;
        }
        let key = book.text();
        self.positions.insert(key.clone(), position);
        self.pending.insert(key, Some(position));
    }

    /// Forgets the position of `book`; `save` writes the removal.
    pub fn delete(&mut self, book: &BookKey) {
        let key = book.text();
        self.positions.remove(&key);
        self.pending.insert(key, None);
    }

    /// Merges the changes made since `load` into the file, which another
    /// reader may have changed in the meantime.
    pub fn save(&mut self) -> Result<()> {
        let Some(path) = self.path.as_deref() else { return Ok(()) };
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut latest = ProgressFile::default();
        storage::update(path, |data| {
            if !data.is_empty() {
                latest = serde_json::from_slice(data).map_err(|error| Error::Format(format!("{}: {error}", path.display())))?;
            }
            for (key, position) in &self.pending {
                match position {
                    Some(position) => latest.positions.insert(key.clone(), *position),
                    None => latest.positions.remove(key),
                };
            }
            serde_json::to_vec_pretty(&latest).map_err(|error| Error::Format(error.to_string()))
        })?;
        self.positions = latest.positions;
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_clamps_and_saves_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yoyomi/progress.json");
        let book = BookKey::new(&dir.path().join("comic.cbz"));
        let mut store = Positions::load_from(Some(path.clone()));
        store.set(&book, Position { page: 4, offset: 2.0, scroll: 7.0, webtoon: true });

        let want = Position { page: 4, offset: 1.0, scroll: 7.0, webtoon: true };
        assert_eq!(store.get(&book), Some(want));
        store.save().unwrap();

        let decoded: ProgressFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(decoded.positions.get(&book.text()), Some(&want));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0, "progress file must be owner-only");
        }

        store.delete(&book);
        assert_eq!(store.get(&book), None);
        store.save().unwrap();
        let decoded: ProgressFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(decoded.positions.is_empty());
        assert_eq!(Positions::load_from(Some(path)).get(&book), None);
    }

    #[test]
    fn stores_preserve_other_changes_and_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let (mut a, mut b) = (Positions::load_from(Some(path.clone())), Positions::load_from(Some(path.clone())));
        let (book_a, book_b) = (&BookKey::new(Path::new("a.cbz")), &BookKey::new(Path::new("b.cbz")));
        a.set(book_a, Position { page: 1, ..Position::default() });
        a.save().unwrap();
        b.set(book_b, Position { page: 2, ..Position::default() });
        b.save().unwrap();
        assert!(b.get(book_a).is_some(), "second writer lost first position");

        a.delete(book_a);
        a.save().unwrap();
        b.set(book_b, Position { page: 3, ..Position::default() });
        b.save().unwrap();
        assert_eq!(b.get(book_a), None, "stale writer resurrected deletion");
        assert_eq!(b.get(book_b).map(|position| position.page), Some(3));
        assert_eq!(Positions::load_from(Some(path)).get(book_b).map(|position| position.page), Some(3));
    }

    #[test]
    fn missing_or_broken_files_load_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        assert_eq!(Positions::load_from(Some(path.clone())).positions.len(), 0);
        fs::write(&path, "{not json").unwrap();
        assert_eq!(Positions::load_from(Some(path.clone())).positions.len(), 0);
        // Saving over a broken file fails rather than drop what it holds.
        let mut store = Positions::load_from(Some(path.clone()));
        store.set(&BookKey::new(Path::new("a.cbz")), Position::default());
        assert!(store.save().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
    }

    // APFS refuses file names that are not UTF-8.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn names_that_are_not_utf8_save_and_load() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let ff = dir.path().join(std::ffi::OsStr::from_bytes(b"\xffodd.cbz"));
        let fe = dir.path().join(std::ffi::OsStr::from_bytes(b"\xfeodd.cbz"));
        let normal = dir.path().join("normal.cbz");
        for book in [&ff, &fe, &normal] {
            fs::write(book, "").unwrap();
        }
        let [ff, fe, normal] = [&ff, &fe, &normal].map(|book| BookKey::new(book));

        let mut store = Positions::load_from(Some(path.clone()));
        store.set(&ff, Position { page: 1, ..Position::default() });
        store.set(&fe, Position { page: 4, ..Position::default() });
        store.set(&normal, Position { page: 2, ..Position::default() });
        assert_eq!(store.get(&ff).map(|position| position.page), Some(1), "0xfe name took the 0xff position");
        store.save().unwrap();
        let mut store = Positions::load_from(Some(path.clone()));
        assert_eq!(store.get(&ff).map(|position| position.page), Some(1));
        assert_eq!(store.get(&fe).map(|position| position.page), Some(4));
        assert_eq!(store.get(&normal).map(|position| position.page), Some(2));

        store.delete(&fe);
        store.set(&normal, Position { page: 3, ..Position::default() });
        store.save().unwrap();
        let store = Positions::load_from(Some(path));
        assert_eq!(store.get(&normal).map(|position| position.page), Some(3));
        assert_eq!(store.get(&ff).map(|position| position.page), Some(1));
        assert_eq!(store.get(&fe), None);
    }

    #[test]
    fn set_replaces_numbers_json_cannot_hold() {
        let mut store = Positions::load_from(None);
        let book = &BookKey::new(Path::new("a.cbz"));
        store.set(book, Position { page: 0, offset: f64::NAN, scroll: f64::INFINITY, webtoon: true });
        assert_eq!(store.get(book), Some(Position { page: 0, offset: 0.0, scroll: 0.0, webtoon: true }));
        store.save().unwrap();
    }
}
