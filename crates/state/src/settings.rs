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
    /// The percent of the pane's width a page takes in theater mode.
    pub theater_width: u8,
    path: Option<PathBuf>,
    /// The glide and the width the file holds, so `save` writes only a
    /// change.
    saved: (u16, u8),
}

/// The contents of `settings.json`. The numbers read as wide as JSON numbers
/// go, so a value far out of range still loads, bounded. A setting the file
/// lacks, as one from before the width does, loads as its default.
#[derive(Serialize, Deserialize)]
struct SettingsFile {
    glide_ms: Option<u64>,
    theater_width: Option<u64>,
}

fn read(path: &Path) -> Option<SettingsFile> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

impl Settings {
    /// The glide without a saved one.
    pub const GLIDE_MS: u16 = 280;
    /// The longest glide.
    pub const MAX_GLIDE_MS: u16 = 600;
    /// The width of a theater page without a saved one: the whole pane.
    pub const THEATER_WIDTH: u8 = 100;
    /// The narrowest theater page.
    pub const MIN_THEATER_WIDTH: u8 = 30;

    /// Reads `settings.json` from the config directory. Missing or broken
    /// data gives the defaults.
    pub fn load() -> Self {
        Self::load_from(crate::config_file("settings.json"))
    }

    /// Reads settings from `path`; `None` keeps them in memory only. A glide
    /// over `MAX_GLIDE_MS` loads as `MAX_GLIDE_MS`, and a width loads within
    /// `MIN_THEATER_WIDTH` and 100.
    pub fn load_from(path: Option<PathBuf>) -> Self {
        let file = path.as_deref().and_then(read);
        let glide_ms = file.as_ref().and_then(|file| file.glide_ms).map_or(Self::GLIDE_MS, |ms| ms.min(Self::MAX_GLIDE_MS.into()) as u16);
        let width =
            file.and_then(|file| file.theater_width).map_or(Self::THEATER_WIDTH, |width| width.clamp(Self::MIN_THEATER_WIDTH.into(), 100) as u8);
        Settings { glide_ms, theater_width: width, path, saved: (glide_ms, width) }
    }

    /// Writes the settings when they changed since `load`. A setting that
    /// did not change keeps the value the file holds by now, which another
    /// reader may have saved.
    pub fn save(&mut self) -> Result<()> {
        let Some(path) = self.path.as_deref() else { return Ok(()) };
        if (self.glide_ms, self.theater_width) == self.saved {
            return Ok(());
        }
        storage::update(path, |latest| {
            let mut file = serde_json::from_slice(latest).unwrap_or(SettingsFile { glide_ms: None, theater_width: None });
            if self.glide_ms != self.saved.0 || file.glide_ms.is_none() {
                file.glide_ms = Some(self.glide_ms.into());
            }
            if self.theater_width != self.saved.1 || file.theater_width.is_none() {
                file.theater_width = Some(self.theater_width.into());
            }
            serde_json::to_vec_pretty(&file).map_err(|error| Error::Format(error.to_string()))
        })?;
        self.saved = (self.glide_ms, self.theater_width);
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
        assert_eq!(Settings::load_from(Some(path.clone())).glide_ms, 0);

        assert_eq!(settings.theater_width, Settings::THEATER_WIDTH);
        settings.theater_width = 80;
        settings.save().unwrap();
        let loaded = Settings::load_from(Some(path));
        assert_eq!((loaded.glide_ms, loaded.theater_width), (0, 80));
    }

    #[test]
    fn missing_broken_or_too_long_glides_load_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(Settings::load_from(Some(path.clone())).glide_ms, Settings::GLIDE_MS);
        for (text, glide_ms) in [
            ("{not json", Settings::GLIDE_MS),
            (r#"{"glide_ms": -40}"#, Settings::GLIDE_MS),
            (r#"{"glide_ms": 900}"#, 600),
            (r#"{"glide_ms": 70000}"#, 600),
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(Settings::load_from(Some(path.clone())).glide_ms, glide_ms, "{text}");
        }
        // A file from before the width keeps its glide, and a width out of
        // range loads bounded.
        for (text, glide_ms, width) in [
            (r#"{"glide_ms": 320}"#, 320, Settings::THEATER_WIDTH),
            (r#"{"glide_ms": 320, "theater_width": 5}"#, 320, Settings::MIN_THEATER_WIDTH),
            (r#"{"glide_ms": 320, "theater_width": 70000}"#, 320, 100),
            (r#"{"glide_ms": 320, "theater_width": 65}"#, 320, 65),
            (r#"{"theater_width": 65}"#, Settings::GLIDE_MS, 65),
        ] {
            fs::write(&path, text).unwrap();
            let loaded = Settings::load_from(Some(path.clone()));
            assert_eq!((loaded.glide_ms, loaded.theater_width), (glide_ms, width), "{text}");
        }
        // Two readers that each change one setting keep both, in either
        // order of saving.
        for glide_first in [true, false] {
            fs::write(&path, r#"{"glide_ms": 280, "theater_width": 100}"#).unwrap();
            let (mut glide, mut width) = (Settings::load_from(Some(path.clone())), Settings::load_from(Some(path.clone())));
            (glide.glide_ms, width.theater_width) = (320, 95);
            let (first, second) = if glide_first { (&mut glide, &mut width) } else { (&mut width, &mut glide) };
            first.save().unwrap();
            second.save().unwrap();
            let loaded = Settings::load_from(Some(path.clone()));
            assert_eq!((loaded.glide_ms, loaded.theater_width), (320, 95), "glide first: {glide_first}");
        }
        // Saving over a broken file replaces it.
        fs::write(&path, "{not json").unwrap();
        let mut settings = Settings::load_from(Some(path.clone()));
        settings.glide_ms = 40;
        settings.save().unwrap();
        assert_eq!(Settings::load_from(Some(path)).glide_ms, 40);
    }
}
