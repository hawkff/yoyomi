//! Pages scaled to the strip width, and the strip images built from them.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use yoyomi_core::{Book, Cancel, Error, PageCache, PageKind, Pixels, Result, RgbaImage};
use yoyomi_render::{invert, rotate, scale};

/// One page as the strip draws it: turned, scaled to `width` x `height` and
/// maybe inverted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub page: usize,
    pub quarter_turns: u8,
    pub width: u32,
    pub height: u32,
    pub invert: bool,
}

impl Key {
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// Scaled pages shared by the strip's jobs. Each page scales once per key:
/// a job that needs a page another job is scaling waits for it, and shares
/// its failure; a later request tries again. The most recently used pages
/// stay within `MAX_PIXELS`.
#[derive(Default)]
pub(super) struct ScaledPages {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// A slot per key; jobs lock a slot while they fill it.
    slots: HashMap<Key, Arc<Mutex<Option<Result<Pixels>>>>>,
    /// Filled keys, least recently used first.
    order: VecDeque<Key>,
    pixels: u64,
    /// How many pages were scaled, for tests.
    scales: usize,
}

impl ScaledPages {
    pub const MAX_PIXELS: u64 = 64_000_000;

    /// The page `key` names, scaled from `book` unless the cache holds it.
    pub fn get(&self, book: &PageCache, key: Key, cancel: &Cancel) -> Result<Pixels> {
        let slot = self.state.lock().slots.entry(key).or_default().clone();
        // Locks go slot first, then state, everywhere.
        let mut filled = slot.lock();
        match filled.as_ref() {
            Some(Ok(pixels)) => {
                self.state.lock().touch(key);
                return Ok(pixels.clone());
            }
            Some(Err(error)) => return Err(error.clone()),
            None => {}
        }
        let result = scale_page(book, key, cancel);
        let mut state = self.state.lock();
        // A clear while the page scaled dropped the slot; the page stays out.
        let kept = state.slots.get(&key).is_some_and(|kept| Arc::ptr_eq(kept, &slot));
        match &result {
            Ok(_) => {
                state.scales += 1;
                *filled = Some(result.clone());
                if kept {
                    state.insert(key);
                }
            }
            // A cancelled job leaves the slot empty for the next one.
            Err(error) if error.is_cancelled() => {}
            // The jobs waiting on this slot share the failure; dropping the
            // slot lets a later request try again.
            Err(_) => {
                *filled = Some(result.clone());
                if kept {
                    state.slots.remove(&key);
                }
            }
        }
        result
    }

    pub fn clear(&self) {
        let mut state = self.state.lock();
        state.slots.clear();
        state.order.clear();
        state.pixels = 0;
    }

    #[cfg(test)]
    pub fn scales(&self) -> usize {
        self.state.lock().scales
    }

    #[cfg(test)]
    pub fn kept(&self) -> usize {
        self.state.lock().order.len()
    }
}

impl State {
    fn touch(&mut self, key: Key) {
        if let Some(i) = self.order.iter().position(|&kept| kept == key) {
            self.order.remove(i);
            self.order.push_back(key);
        }
    }

    fn insert(&mut self, key: Key) {
        self.pixels += key.pixels();
        self.order.push_back(key);
        // The newest page always stays.
        while self.pixels > ScaledPages::MAX_PIXELS && self.order.len() > 1 {
            let Some(oldest) = self.order.pop_front() else { break };
            self.slots.remove(&oldest);
            self.pixels -= oldest.pixels();
        }
    }
}

/// Decodes, turns, scales and inverts one page, copying it only when one of
/// those changes it.
fn scale_page(book: &PageCache, key: Key, cancel: &Cancel) -> Result<Pixels> {
    let page = book.page(key.page, cancel)?;
    let turned = (key.quarter_turns & 3 != 0).then(|| rotate(&page, key.quarter_turns));
    let source = turned.as_ref().unwrap_or(&page);
    let mut changed = if source.dimensions() == (key.width, key.height) { turned } else { Some(scale(source, key.width, key.height)) };
    if key.invert {
        invert(changed.get_or_insert_with(|| RgbaImage::clone(&page)));
    }
    Ok(changed.map_or(page, Arc::new))
}

/// One page in a strip image: its scaled pixels go with their top-left
/// corner at `x`, `y` of the image.
#[derive(Clone, Copy, Debug)]
pub(super) struct Piece {
    pub key: Key,
    pub x: u32,
    pub y: i64,
}

/// A transparent `width` x `height` image with the rows of `pieces` copied
/// in. Pages that fail to load stay transparent and report why. With
/// `refuse_text`, as for half blocks, which would scale the text into
/// unreadable blocks, a text page fails the whole image.
pub(super) fn compose(
    book: &PageCache,
    scaled: &ScaledPages,
    pieces: &[Piece],
    (width, height): (u32, u32),
    refuse_text: bool,
    cancel: &Cancel,
) -> Result<(RgbaImage, Vec<String>)> {
    if refuse_text && pieces.iter().any(|piece| book.kind(piece.key.page) == PageKind::Text) {
        return Err(Error::unsupported(TEXT_NEEDS_PIXELS));
    }
    let mut image = RgbaImage::new(width, height);
    let mut errors = Vec::new();
    for piece in pieces {
        match scaled.get(book, piece.key, cancel) {
            Ok(pixels) => paint(&mut image, &pixels, piece.x, piece.y),
            Err(error) if error.is_cancelled() => return Err(error),
            Err(error) => errors.push(format!("page {}: {error}", piece.key.page + 1)),
        }
    }
    Ok((image, errors))
}

/// The message for text pages in half-block webtoon mode. macOS builds have
/// the native window that `f` opens.
pub(super) const TEXT_NEEDS_PIXELS: &str = if cfg!(target_os = "macos") {
    "text pages in webtoon need pixel rendering: t to leave webtoon, e for browser, f for native, or a terminal with kitty or sixel graphics"
} else {
    "text pages in webtoon need pixel rendering: t to leave webtoon, e for browser, or a terminal with kitty or sixel graphics"
};

/// Copies the rows of `source` that fall inside `image`, with its top-left
/// corner at `x`, `y`.
fn paint(image: &mut RgbaImage, source: &RgbaImage, x: u32, y: i64) {
    let (width, height) = (image.width() as usize, i64::from(image.height()));
    let (x, stride) = (x as usize, source.width() as usize * 4);
    let span = (source.width() as usize).min(width.saturating_sub(x)) * 4;
    let (from, to) = (y.max(0), (y + i64::from(source.height())).min(height));
    let (source, image) = (source.as_raw(), &mut **image);
    for row in from..to {
        let src = (row - y) as usize * stride;
        let dst = (row as usize * width + x) * 4;
        image[dst..dst + span].copy_from_slice(&source[src..src + span]);
    }
}
