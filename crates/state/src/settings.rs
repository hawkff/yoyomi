//! What the reader keeps between runs besides positions and bookmarks.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use yoyomi_core::{Error, Result};

use crate::storage;

/// The reader's settings.
#[derive(Debug)]
pub struct Settings {
    /// How long a move takes in webtoon and theater mode where the picture
    /// slides, in milliseconds; 0 moves at once.
    pub glide_ms: u16,
    path: Option<PathBuf>,
    /// The glide the file holds, so `save` writes only a change.
    saved: u16,
}

/// The contents of `settings.json`.
#[derive(Serialize, Deserialize)]
struct SettingsFile {
    glide_ms: u16,
}

fn read(path: &Path) -> Option<SettingsFile> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

impl Settings {
    /// The glide without a saved one.
    pub const GLIDE_MS: u16 = 280;
    /// The longest glide.
    pub const MAX_GLIDE_MS: u16 = 600;

    /// Reads `settings.json` from the config directory. Missing or broken
    /// data gives the defaults.
    pub fn load() -> Self {
        Self::load_from(crate::config_file("settings.json"))
    }

    /// Reads settings from `path`; `None` keeps them in memory only. A glide
    /// over `MAX_GLIDE_MS` loads as `MAX_GLIDE_MS`.
    pub fn load_from(path: Option<PathBuf>) -> Self {
        let glide_ms = path.as_deref().and_then(read).map_or(Self::GLIDE_MS, |file| file.glide_ms.min(Self::MAX_GLIDE_MS));
        Settings { glide_ms, path, saved: glide_ms }
    }

    /// Writes the settings when they changed since `load`.
    pub fn save(&mut self) -> Result<()> {
        let Some(path) = self.path.as_deref() else { return Ok(()) };
        if self.glide_ms == self.saved {
            return Ok(());
        }
        let file = SettingsFile { glide_ms: self.glide_ms };
        storage::update(path, |_| serde_json::to_vec_pretty(&file).map_err(|error| Error::Format(error.to_string())))?;
        self.saved = self.glide_ms;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_glide_saves_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yoyomi/settings.json");
        let mut settings = Settings::load_from(Some(path.clone()));
        assert_eq!(settings.glide_ms, Settings::GLIDE_MS);
        settings.save().unwrap();
        assert!(!path.exists(), "unchanged settings write nothing");

        settings.glide_ms = 320;
        settings.save().unwrap();
        assert_eq!(Settings::load_from(Some(path.clone())).glide_ms, 320);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0, "settings file must be owner-only");
        }
        settings.glide_ms = 0;
        settings.save().unwrap();
        assert_eq!(Settings::load_from(Some(path)).glide_ms, 0);
    }

    #[test]
    fn missing_broken_or_too_long_glides_load_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(Settings::load_from(Some(path.clone())).glide_ms, Settings::GLIDE_MS);
        for (text, glide_ms) in [("{not json", Settings::GLIDE_MS), (r#"{"glide_ms": -40}"#, Settings::GLIDE_MS), (r#"{"glide_ms": 900}"#, 600)] {
            fs::write(&path, text).unwrap();
            assert_eq!(Settings::load_from(Some(path.clone())).glide_ms, glide_ms, "{text}");
        }
        // Saving over a broken file replaces it.
        fs::write(&path, "{not json").unwrap();
        let mut settings = Settings::load_from(Some(path.clone()));
        settings.glide_ms = 40;
        settings.save().unwrap();
        assert_eq!(Settings::load_from(Some(path)).glide_ms, 40);
    }
}
