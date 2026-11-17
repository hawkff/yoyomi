//! Pages scaled to the strip width in sections of rows, and the strip images
//! built from them.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use parking_lot::Mutex;
use yoyomi_core::{Book, Cancel, Error, PageCache, PageKind, Pixels, Result, RgbaImage, check_size};
use yoyomi_render::{invert, scale_part};

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

/// Rows of a scaled page per section.
pub(super) const SECTION_ROWS: u32 = 1024;

/// Section `index` of the page `key` names: at most `SECTION_ROWS` of its
/// scaled rows, from row `index * SECTION_ROWS` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Section {
    pub key: Key,
    pub index: u32,
}

impl Section {
    /// The rows of the scaled page it holds.
    fn rows(&self) -> Range<u32> {
        let top = self.index * SECTION_ROWS;
        top..top.saturating_add(SECTION_ROWS).min(self.key.height)
    }

    fn pixels(&self) -> u64 {
        let rows = self.rows();
        u64::from(self.key.width) * u64::from(rows.end - rows.start)
    }
}

/// Rows of a scaled page: row 0 of `pixels` is row `top` of the page.
#[derive(Clone)]
pub(super) struct ScaledRows {
    pub pixels: Pixels,
    pub top: u32,
}

/// Scaled sections of pages shared by the strip's jobs. Each section scales
/// once: a job that needs a section another job is scaling waits for it, and
/// shares its failure; a later request tries again. The most recently used
/// sections stay within `MAX_PIXELS`.
#[derive(Default)]
pub(super) struct ScaledPages {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// A slot per section; jobs lock a slot while they fill it.
    slots: HashMap<Section, Arc<Mutex<Option<Result<ScaledRows>>>>>,
    /// Filled sections, least recently used first.
    order: VecDeque<Section>,
    pixels: u64,
    /// How many sections were scaled, for tests.
    scales: usize,
}

impl ScaledPages {
    pub const MAX_PIXELS: u64 = 64_000_000;

    /// The rows of `section`, scaled from `book` unless the cache holds them.
    pub fn get(&self, book: &PageCache, section: Section, cancel: &Cancel) -> Result<ScaledRows> {
        let slot = self.state.lock().slots.entry(section).or_default().clone();
        // Locks go slot first, then state, everywhere.
        let mut filled = slot.lock();
        match filled.as_ref() {
            Some(Ok(rows)) => {
                self.state.lock().touch(section);
                return Ok(rows.clone());
            }
            Some(Err(error)) => return Err(error.clone()),
            None => {}
        }
        let result = scale_section(book, section, cancel);
        let mut state = self.state.lock();
        // A clear while the section scaled dropped the slot; the section
        // stays out.
        let kept = state.slots.get(&section).is_some_and(|kept| Arc::ptr_eq(kept, &slot));
        match &result {
            Ok(_) => {
                state.scales += 1;
                *filled = Some(result.clone());
                if kept {
                    state.insert(section);
                }
            }
            // A cancelled job leaves the slot empty for the next one.
            Err(error) if error.is_cancelled() => {}
            // The jobs waiting on this slot share the failure; dropping the
            // slot lets a later request try again.
            Err(_) => {
                *filled = Some(result.clone());
                if kept {
                    state.slots.remove(&section);
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

    #[cfg(test)]
    pub fn pixels(&self) -> u64 {
        self.state.lock().pixels
    }
}

impl State {
    fn touch(&mut self, section: Section) {
        if let Some(i) = self.order.iter().position(|&kept| kept == section) {
            self.order.remove(i);
            self.order.push_back(section);
        }
    }

    fn insert(&mut self, section: Section) {
        self.pixels += section.pixels();
        self.order.push_back(section);
        // The newest section always stays.
        while self.pixels > ScaledPages::MAX_PIXELS && self.order.len() > 1 {
            let Some(oldest) = self.order.pop_front() else { break };
            self.slots.remove(&oldest);
            self.pixels -= oldest.pixels();
        }
    }
}

/// Decodes one page and turns, scales and inverts the rows of `section`. A
/// page that none of those change comes whole, without a copy.
fn scale_section(book: &PageCache, section: Section, cancel: &Cancel) -> Result<ScaledRows> {
    let (key, rows) = (section.key, section.rows());
    check_size(key.width, rows.end - rows.start)?;
    let page = book.page(key.page, cancel)?;
    if key.quarter_turns & 3 == 0 && !key.invert && page.dimensions() == (key.width, key.height) {
        return Ok(ScaledRows { pixels: page, top: 0 });
    }
    let mut pixels = scale_part(&page, key.quarter_turns, (key.width, key.height), (0, rows.start, key.width, rows.end - rows.start));
    if key.invert {
        invert(&mut pixels);
    }
    Ok(ScaledRows { pixels: Arc::new(pixels), top: rows.start })
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
    for &Piece { key, x, y } in pieces {
        // Only the sections of the rows inside the image scale.
        let rows = |at: i64| (at - y).clamp(0, i64::from(key.height)) as u32;
        for index in rows(0) / SECTION_ROWS..rows(i64::from(height)).div_ceil(SECTION_ROWS) {
            let section = Section { key, index };
            match scaled.get(book, section, cancel) {
                Ok(part) => paint(&mut image, &part, section.rows(), x, y),
                Err(error) if error.is_cancelled() => return Err(error),
                Err(error) => {
                    errors.push(format!("page {}: {error}", key.page + 1));
                    break;
                }
            }
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

/// Copies the page rows `rows` of `scaled` that fall inside `image`, with the
/// page's top-left corner at `x`, `y`.
fn paint(image: &mut RgbaImage, scaled: &ScaledRows, rows: Range<u32>, x: u32, y: i64) {
    let source = &scaled.pixels;
    let (width, height) = (image.width() as usize, i64::from(image.height()));
    let (x, stride) = (x as usize, source.width() as usize * 4);
    let span = (source.width() as usize).min(width.saturating_sub(x)) * 4;
    let (from, to) = ((y + i64::from(rows.start)).max(0), (y + i64::from(rows.end)).min(height));
    let (top, source, image) = (y + i64::from(scaled.top), source.as_raw(), &mut **image);
    for row in from..to {
        let src = (row - top) as usize * stride;
        let dst = (row as usize * width + x) * 4;
        image[dst..dst + span].copy_from_slice(&source[src..src + span]);
    }
}
