//! Screenshots: the shown page, turned, zoomed and inverted as on screen,
//! saved as PNG.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use yoyomi_core::image::ImageFormat;
use yoyomi_core::{Book, Cancel, Result, Transform};
use yoyomi_render::{apply, invert};

/// Saves page `page` of `book` with `transform` into `dir` and returns the
/// file's path.
pub fn save(book: &dyn Book, page: usize, transform: &Transform, dir: &Path) -> Result<PathBuf> {
    let pixels = book.page(page, &Cancel::new())?;
    let transform = Transform { invert: transform.invert && book.can_invert(page), ..transform.clone() };
    let (image, inverted) = apply(&pixels, &transform);
    let mut image = Arc::unwrap_or_clone(image);
    if inverted {
        invert(&mut image);
    }
    let path = dir.join(name(book.title(), page));
    image.save_with_format(&path, ImageFormat::Png)?;
    Ok(path)
}

/// The file name of the screenshot of page index `page`.
fn name(title: &str, page: usize) -> String {
    format!("yoyomi-{}-p{:03}.png", sanitize(title), page + 1)
}

/// Keeps letters and digits of any script, so screenshots of books with
/// Cyrillic or Japanese titles get distinct names.
fn sanitize(title: &str) -> String {
    let name: Vec<char> = title
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('_'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect();
    // 56 characters of up to 4 bytes leave room for the rest of the name and
    // any page number within a 255-byte file name.
    if name.len() <= 56 {
        return name.into_iter().collect();
    }
    // Long titles keep a checksum of the rest, so volumes of a series stay apart.
    let whole: String = name.iter().collect();
    format!("{}-{:08x}", name[..50].iter().collect::<String>(), crc32(whole.as_bytes()))
}

/// The IEEE CRC-32 of `bytes`: the reflected polynomial `0xEDB88320`,
/// started from all ones and inverted at the end.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::FakeBook;

    #[test]
    fn names_keep_every_script() {
        assert_eq!(sanitize("Война и мир: том 1 / 漫画?"), "Война_и_мир_том_1__漫画");
        let volume = "漫".repeat(70);
        let (first, second) = (sanitize(&format!("{volume}1")), sanitize(&format!("{volume}2")));
        assert_ne!(first, second);
        assert_eq!(first.chars().count(), 59);
        assert!(first.starts_with(&format!("{}-", "漫".repeat(50))));
        // Four-byte letters and a five-digit page still fit a 255-byte name.
        assert!(name(&"\u{10400}".repeat(60), 9999).len() <= 255);
        assert_eq!(name("Fake", 2), "yoyomi-Fake-p003.png");
    }

    #[test]
    fn the_checksum_is_crc32_ieee() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn saves_the_page_as_png() {
        let dir = tempfile::tempdir().unwrap();
        let book = FakeBook { len: 3, path: "/books/fake.cbz".into() };
        let path = save(&book, 1, &Transform::default(), dir.path()).unwrap();
        assert_eq!(path, dir.path().join("yoyomi-Fake-p002.png"));
        assert_eq!(std::fs::read(&path).unwrap()[..8], *b"\x89PNG\r\n\x1a\n");
        assert!(save(&book, 7, &Transform::default(), dir.path()).is_err());
    }
}
