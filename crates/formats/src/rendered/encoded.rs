//! PNG copies of PPM pages for the browser reader, which cannot show the
//! PPM that pdftoppm and ddjvu print or that a comic archive holds.

use std::collections::VecDeque;

use yoyomi_core::image::codecs::png::{CompressionType, FilterType, PngEncoder};
use yoyomi_core::image::{ExtendedColorType, ImageEncoder};
use yoyomi_core::{Encoded, MAX_PAGE_BYTES, Result, RgbaImage};

/// How many converted pages one book keeps.
const CACHED_PAGES: usize = 8;

/// The converted pages of one book, oldest first, within `CACHED_PAGES`
/// pages and `MAX_PAGE_BYTES` bytes.
#[derive(Default)]
pub(super) struct EncodedPages {
    pages: VecDeque<(usize, Encoded)>,
    bytes: usize,
}

impl EncodedPages {
    pub(super) fn get(&self, page: usize) -> Option<Encoded> {
        self.pages.iter().find(|(cached, _)| *cached == page).map(|(_, encoded)| encoded.clone())
    }

    /// Adds a page that `get` did not find and evicts the oldest pages past
    /// the limits.
    pub(super) fn insert(&mut self, page: usize, encoded: Encoded) {
        self.bytes += encoded.bytes.len();
        self.pages.push_back((page, encoded));
        while self.pages.len() > CACHED_PAGES || self.bytes > MAX_PAGE_BYTES {
            let Some((_, evicted)) = self.pages.pop_front() else { break };
            self.bytes -= evicted.bytes.len();
        }
    }
}

/// Encodes `pixels` as a PNG, favoring speed over size.
pub(crate) fn png(pixels: &RgbaImage) -> Result<Encoded> {
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive).write_image(
        pixels.as_raw(),
        pixels.width(),
        pixels.height(),
        ExtendedColorType::Rgba8,
    )?;
    Ok(Encoded { bytes: out.into(), mime: "image/png" })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn page(len: usize) -> Encoded {
        Encoded { bytes: vec![0; len].into(), mime: "image/png" }
    }

    #[test]
    fn the_oldest_pages_leave_first() {
        let mut pages = EncodedPages::default();
        for i in 0..=CACHED_PAGES {
            pages.insert(i, page(10));
        }
        assert!(pages.get(0).is_none());
        assert!((1..=CACHED_PAGES).all(|i| pages.get(i).is_some()));
        assert_eq!(pages.bytes, CACHED_PAGES * 10);
        pages.insert(CACHED_PAGES + 1, page(MAX_PAGE_BYTES - 10));
        assert_eq!((pages.pages.len(), pages.bytes), (2, MAX_PAGE_BYTES), "the byte limit evicts too");
    }

    #[test]
    fn a_cached_page_shares_its_bytes() {
        let mut pages = EncodedPages::default();
        let encoded = page(3);
        pages.insert(4, encoded.clone());
        assert!(Arc::ptr_eq(&pages.get(4).unwrap().bytes, &encoded.bytes));
    }
}
