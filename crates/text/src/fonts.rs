//! The fonts text pages use: the bundled Go fonts (regular, bold, italic,
//! bold italic) first, then fallback fonts for the scripts they lack.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use cosmic_text::FontSystem;
use fontdb::{Database, Source};
use yoyomi_core::{Error, Result};

/// The family name of the bundled fonts.
pub(crate) const BUNDLED_FAMILY: &str = "Go";

const BUNDLED: [&[u8]; 4] = [
    include_bytes!("../fonts/Go-Regular.ttf"),
    include_bytes!("../fonts/Go-Bold.ttf"),
    include_bytes!("../fonts/Go-Italic.ttf"),
    include_bytes!("../fonts/Go-Bold-Italic.ttf"),
];

const MAX_FONT_BYTES: u64 = 32 << 20;
const MAX_FONT_FACES: u32 = 32;

/// Fonts for symbols and CJK where the system packages put them.
#[cfg(target_os = "macos")]
const SYSTEM_FALLBACKS: &[&str] = &[
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
];

/// DejaVu Sans, Noto Sans Symbols 2 and Noto Sans CJK where the Debian,
/// Ubuntu, Void, Fedora, Arch, Alpine and FreeBSD packages put them.
#[cfg(not(target_os = "macos"))]
const SYSTEM_FALLBACKS: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/local/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/google-noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
];

/// A font system for one book. The font files load once per process;
/// `YOYOMI_EPUB_FALLBACK_FONT` names a font file that replaces the system
/// fallbacks.
pub(crate) fn font_system() -> Result<FontSystem> {
    static DATABASE: OnceLock<Result<Database, String>> = OnceLock::new();
    let database = DATABASE.get_or_init(|| database(std::env::var_os("YOYOMI_EPUB_FALLBACK_FONT").as_deref()));
    let database = database.as_ref().map_err(|e| Error::Format(e.clone()))?;
    Ok(FontSystem::new_with_locale_and_db("en-US".to_owned(), database.clone()))
}

/// The bundled fonts, then the font file `configured` names or else the
/// system fallbacks that exist.
fn database(configured: Option<&OsStr>) -> Result<Database, String> {
    let mut database = Database::new();
    for data in BUNDLED {
        database.load_font_source(Source::Binary(Arc::new(data)));
    }
    match configured.filter(|path| !path.is_empty()) {
        Some(path) => load(&mut database, Path::new(path), MAX_FONT_FACES).map_err(|e| format!("EPUB fallback font: {e}"))?,
        // The faces of these collections are regional variants of the same
        // characters, so the first face is enough.
        None => SYSTEM_FALLBACKS.iter().for_each(|path| drop(load(&mut database, Path::new(path), 1))),
    }
    Ok(database)
}

/// Adds up to `faces` faces of the TTF, OTF or collection at `path`.
fn load(database: &mut Database, path: &Path, faces: u32) -> Result<(), String> {
    let mut data = Vec::new();
    File::open(path).and_then(|file| file.take(MAX_FONT_BYTES + 1).read_to_end(&mut data)).map_err(|e| e.to_string())?;
    if data.len() as u64 > MAX_FONT_BYTES {
        return Err(format!("data exceeds {MAX_FONT_BYTES} bytes"));
    }
    if !matches!(data.get(..4), Some(b"\x00\x01\x00\x00" | b"OTTO" | b"true" | b"typ1" | b"ttcf")) {
        return Err("font must be TTF/OTF or TTC/OTC".to_owned());
    }
    if let Some(count) = data.get(8..12).filter(|_| data.starts_with(b"ttcf")) {
        let count = u32::from_be_bytes(count.try_into().expect("four bytes"));
        if !(1..=MAX_FONT_FACES).contains(&count) {
            return Err(format!("font collection must contain 1 to {MAX_FONT_FACES} faces"));
        }
    }
    let before: Vec<_> = database.faces().map(|face| face.id).collect();
    database.load_font_source(Source::Binary(Arc::new(data)));
    let added: Vec<_> = database.faces().filter(|face| !before.contains(&face.id)).map(|face| (face.id, face.index)).collect();
    if added.is_empty() {
        return Err("font has no readable faces".to_owned());
    }
    for (id, index) in added {
        if index >= faces {
            database.remove_face(id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_fallback_font_replaces_the_system_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fallback.ttf");
        assert!(database(Some(path.as_os_str())).is_err(), "missing configured font ignored");
        std::fs::write(&path, b"not a font").unwrap();
        assert!(database(Some(path.as_os_str())).is_err(), "invalid configured font ignored");
        std::fs::write(&path, BUNDLED[0]).unwrap();
        assert_eq!(database(Some(path.as_os_str())).unwrap().len(), BUNDLED.len() + 1);
        File::options().write(true).open(&path).unwrap().set_len(MAX_FONT_BYTES + 1).unwrap();
        assert!(database(Some(path.as_os_str())).unwrap_err().contains("exceeds"));
    }

    /// A collection of `count` copies of the bundled regular face.
    fn collection(count: u32) -> Vec<u8> {
        let font = BUNDLED[0];
        let header = 12 + 4 * count as usize;
        let mut data = vec![0; header];
        data[..4].copy_from_slice(b"ttcf");
        data[4..8].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
        data[8..12].copy_from_slice(&count.to_be_bytes());
        for i in 0..count as usize {
            data[12 + 4 * i..16 + 4 * i].copy_from_slice(&(header as u32).to_be_bytes());
        }
        data.extend_from_slice(font);
        let tables = usize::from(u16::from_be_bytes([font[4], font[5]]));
        for i in 0..tables {
            let at = header + 12 + 16 * i + 8;
            let offset = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) + header as u32;
            data[at..at + 4].copy_from_slice(&offset.to_be_bytes());
        }
        data
    }

    #[test]
    fn collections_load_within_the_face_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fallback.ttc");
        std::fs::write(&path, collection(2)).unwrap();
        assert_eq!(database(Some(path.as_os_str())).unwrap().len(), BUNDLED.len() + 2);
        let mut first = Database::new();
        load(&mut first, &path, 1).unwrap();
        assert_eq!(first.len(), 1, "system collections keep their first face");
        std::fs::write(&path, collection(MAX_FONT_FACES + 1)).unwrap();
        assert!(database(Some(path.as_os_str())).unwrap_err().contains("faces"));
    }
}
