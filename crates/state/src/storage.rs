//! Locked read-modify-write updates of small JSON files, and the keys they
//! file books under.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use yoyomi_core::Result;

/// Rewrites `path` under a lock file next to it: reads the file (empty when
/// it does not exist), passes the bytes to `apply`, writes the result to a
/// temporary file and renames that over `path`.
pub(crate) fn update(path: &Path, apply: impl FnOnce(&[u8]) -> Result<Vec<u8>>) -> Result<()> {
    update_then_sync(path, apply, sync_dir)
}

fn update_then_sync(path: &Path, apply: impl FnOnce(&[u8]) -> Result<Vec<u8>>, sync: fn(&Path) -> io::Result<()>) -> Result<()> {
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let lock = owner_only().create(true).write(true).truncate(false).open(with_suffix(path, ".lock"))?;
    // Closing the file when it drops releases the lock.
    lock.lock()?;
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let data = apply(&data)?;
    // Only the lock holder writes this name, so it needs no random part.
    let temp = with_suffix(path, ".tmp");
    let written = owner_only().create(true).write(true).truncate(true).open(&temp).and_then(|mut file| {
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    });
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    // The rename already committed the new file, so a failed sync must not report the save as failed.
    let _ = sync(dir);
    Ok(())
}

/// Makes a rename in `dir` durable, so a power loss cannot bring back the old
/// file after a save returned.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// The key positions and bookmarks file a book under, so every spelling of a
/// path finds the same entry: the canonical path of an existing file, else
/// the absolute path with `.` and `..` resolved by name, as for a book that
/// is gone. Making one asks the file system, so a reader makes it once when
/// it opens the book.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BookKey(PathBuf);

impl BookKey {
    pub fn new(book: &Path) -> Self {
        BookKey(fs::canonicalize(book).unwrap_or_else(|_| clean_absolute(book)))
    }

    /// The path the key names, which opens the book.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The key as the saved files hold it.
    pub(crate) fn text(&self) -> String {
        key_text(&self.0)
    }
}

impl Serialize for BookKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for BookKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        // A saved key was made when it was saved, so loading takes it as it is.
        String::deserialize(deserializer).map(|text| BookKey(key_path(&text)))
    }
}

/// Marks a key saved as the hex of its bytes; no absolute path starts with it.
#[cfg(unix)]
const BYTES_PREFIX: &str = "bytes:";

/// A key as the saved files hold it, since JSON holds only text: a UTF-8 path
/// as itself, any other as `bytes:` and the lowercase hex of its bytes, so
/// different names never share a key.
#[cfg(unix)]
fn key_text(key: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    match key.to_str() {
        Some(text) => text.to_owned(),
        None => BYTES_PREFIX.to_owned() + &key.as_os_str().as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
    }
}

#[cfg(not(unix))]
fn key_text(key: &Path) -> String {
    key.to_string_lossy().into_owned()
}

/// The key `key_text` saved as `text`.
#[cfg(unix)]
fn key_path(text: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let nibble = |digit: u8| match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    };
    let bytes = text
        .strip_prefix(BYTES_PREFIX)
        .filter(|hex| hex.len() % 2 == 0)
        .and_then(|hex| hex.as_bytes().chunks(2).map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?)).collect::<Option<Vec<u8>>>());
    match bytes {
        Some(bytes) => OsString::from_vec(bytes).into(),
        None => text.into(),
    }
}

#[cfg(not(unix))]
fn key_path(text: &str) -> PathBuf {
    text.into()
}

fn clean_absolute(book: &Path) -> PathBuf {
    let path = std::path::absolute(book).unwrap_or_else(|_| book.to_path_buf());
    let mut key = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match key.components().next_back() {
                Some(Component::Normal(_)) => {
                    key.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => key.push(component),
            },
            _ => key.push(component),
        }
    }
    key
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path);
    name.push(suffix);
    name.into()
}

/// Options for files only their owner may read: the saved files name the
/// books a reader opened.
fn owner_only() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

#[cfg(test)]
mod tests {
    use std::thread;

    use yoyomi_core::Error;

    use super::*;

    #[test]
    fn concurrent_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("count.json");
        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    update(&path, |data| {
                        let count: u32 = if data.is_empty() {
                            0
                        } else {
                            std::str::from_utf8(data).unwrap().parse().map_err(|_| Error::Format("not a count".into()))?
                        };
                        Ok((count + 1).to_string().into_bytes())
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(fs::read_to_string(&path).unwrap(), "8");
        assert!(!with_suffix(&path, ".tmp").exists());
    }

    #[test]
    fn failed_apply_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/data.json");
        update(&path, |_| Ok(b"old".to_vec())).unwrap();
        assert!(update(&path, |_| Err(Error::Format("broken".into()))).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
    }

    #[test]
    fn failed_directory_sync_keeps_the_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        update_then_sync(&path, |_| Ok(b"new".to_vec()), |_| Err(io::Error::other("sync failed"))).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn key_text_keeps_names_that_are_not_utf8_apart() {
        use std::os::unix::ffi::OsStrExt;
        let (ff, fe) = (Path::new(std::ffi::OsStr::from_bytes(b"/a\xff")), Path::new(std::ffi::OsStr::from_bytes(b"/a\xfe")));
        assert_eq!(key_text(ff), "bytes:2f61ff");
        assert_eq!(key_text(fe), "bytes:2f61fe");
        assert_eq!(key_text(Path::new("/a")), "/a");
        for key in [ff, fe, Path::new("/a"), Path::new("bytes:zz")] {
            assert_eq!(key_path(&key_text(key)), key);
        }
    }

    #[cfg(unix)]
    #[test]
    fn sync_dir_opens_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        sync_dir(dir.path()).unwrap();
        assert!(sync_dir(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn keys_are_absolute_and_clean() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(BookKey::new(Path::new("x/./../a.cbz")).path(), cwd.join("a.cbz"));
        let root = Path::new("/");
        assert_eq!(BookKey::new(&root.join("../b.cbz")).path(), root.join("b.cbz"));
    }

    #[cfg(unix)]
    #[test]
    fn keys_follow_symlinks_before_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let (library, other) = (dir.path().join("library"), dir.path().join("other"));
        fs::create_dir_all(other.join("sub")).unwrap();
        fs::create_dir(&library).unwrap();
        std::os::unix::fs::symlink(other.join("sub"), library.join("link")).unwrap();
        fs::write(library.join("a.cbz"), "").unwrap();
        fs::write(other.join("a.cbz"), "").unwrap();

        let through_link = BookKey::new(&library.join("link/../a.cbz"));
        assert_eq!(through_link.path(), fs::canonicalize(other.join("a.cbz")).unwrap());
        assert_ne!(through_link, BookKey::new(&library.join("a.cbz")));
    }
}
