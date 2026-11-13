//! Reading positions and bookmarks, kept as JSON files in the user config
//! directory. Every update holds a lock file across reading, merging and
//! renaming, so two readers never lose each other's changes.

mod bookmarks;
mod positions;
mod storage;
mod time;

use std::env;
use std::path::PathBuf;

pub use bookmarks::{Bookmarks, Mark};
pub use positions::{Position, Positions};
pub use storage::BookKey;

/// The directory yoyomi keeps its files in: `$XDG_CONFIG_HOME/yoyomi`, by
/// default `~/.config/yoyomi`, and `~/Library/Application Support/yoyomi` on
/// macOS. `None` when neither the variable nor `$HOME` is set.
pub fn config_dir() -> Option<PathBuf> {
    let home = || env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home()?.join("Library/Application Support")
    } else {
        match env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            Some(dir) if dir.is_absolute() => dir,
            _ => home()?.join(".config"),
        }
    };
    Some(base.join("yoyomi"))
}

/// The file `name` in the config directory.
fn config_file(name: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(name))
}
