use std::path::Path;
use std::sync::Arc;

use image::RgbaImage;

use crate::{Cancel, Error, Result};

/// A decoded page, shared read-only between caches and renderers.
pub type Pixels = Arc<RgbaImage>;

/// Where a page comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PageKind {
    /// A picture stored in the book: a comic page or an illustration.
    Image,
    /// A page an external tool draws on demand, as for PDF and DJVU.
    Rendered,
    /// A page laid out from the book's text.
    Text,
}

/// The first page of a chapter in a book's outline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chapter {
    pub title: String,
    /// Zero-based page index.
    pub page: usize,
    /// Zero-based outline level.
    pub depth: usize,
}

/// The chapter that holds `page`: the nearest preceding destination,
/// regardless of outline order. Later entries win ties, so a subsection takes
/// precedence over its parent.
pub fn current_chapter(chapters: &[Chapter], page: usize) -> Option<usize> {
    let mut current: Option<usize> = None;
    for (i, chapter) in chapters.iter().enumerate() {
        if chapter.page <= page && current.is_none_or(|c| chapter.page >= chapters[c].page) {
            current = Some(i);
        }
    }
    current
}

/// Where a link leads.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Link {
    /// A web or mail address.
    Url(String),
    /// A zero-based page of the same book.
    Page(usize),
}

/// A glyph or word on a line of text, in page fractions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextBox {
    pub x0: f64,
    pub x1: f64,
    /// The box's first rune in its line's text.
    pub index: usize,
    /// How many runes the box covers.
    pub count: usize,
    pub rtl: bool,
}

/// A link over a range of runes on one line.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkSpan {
    pub start: usize,
    pub end: usize,
    pub target: Link,
}

/// One line of selectable text, in page fractions.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLine {
    pub text: String,
    pub top: f64,
    pub bottom: f64,
    /// The line ends its paragraph.
    pub end: bool,
    pub boxes: Vec<TextBox>,
    pub links: Vec<LinkSpan>,
}

/// The text of one page, for selection, links and search.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextLayer {
    pub lines: Vec<TextLine>,
}

/// A page as stored bytes, for the browser reader.
#[derive(Clone, Debug)]
pub struct Encoded {
    pub bytes: Arc<[u8]>,
    /// The MIME type, such as `image/jpeg`.
    pub mime: &'static str,
}

/// One open book: a comic archive, a rendered document or a laid-out text.
///
/// Every method takes `&self`, and implementations synchronize internally,
/// so one book serves the pager, the strip and background jobs at once.
pub trait Book: Send + Sync {
    /// The absolute path the book was opened from.
    fn path(&self) -> &Path;

    /// The file name without its extension.
    fn title(&self) -> &str;

    /// The number of pages.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Where page `page` comes from.
    fn kind(&self, page: usize) -> PageKind;

    /// Whether inversion applies to `page`. Comic, PDF and DJVU pages invert
    /// whole; text books invert their text pages and keep illustrations.
    fn can_invert(&self, page: usize) -> bool {
        page < self.len()
    }

    /// Decodes page `page`. Blocks until the pixels are ready or `cancel`
    /// fires.
    fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels>;

    /// The pixel size of `page`. The default decodes the page; books that
    /// can read a header or ask a tool do that instead.
    fn page_size(&self, page: usize, cancel: &Cancel) -> Result<(u32, u32)> {
        self.page(page, cancel).map(|pixels| pixels.dimensions())
    }

    /// The book's outline, or an empty list.
    fn chapters(&self, cancel: &Cancel) -> Result<Vec<Chapter>> {
        cancel.check()?;
        Ok(Vec::new())
    }

    /// The selectable text of `page`, when the page has any.
    fn text_layer(&self, page: usize, cancel: &Cancel) -> Result<Option<Arc<TextLayer>>> {
        let _ = page;
        cancel.check()?;
        Ok(None)
    }

    /// The text search reads on `page` without OCR, for books that read it
    /// apart from their text layers, as PDFs do. `None` leaves search to
    /// the page's text layer.
    fn search_text(&self, page: usize, cancel: &Cancel) -> Result<Option<String>> {
        let _ = page;
        cancel.check()?;
        Ok(None)
    }

    /// The paragraphs of a text page, for renderers without pixel graphics.
    fn page_text(&self, page: usize) -> Option<Vec<String>> {
        let _ = page;
        None
    }

    /// The stored bytes of `page`, for the browser reader.
    fn encoded(&self, page: usize, cancel: &Cancel) -> Result<Encoded> {
        let _ = (page, cancel);
        Err(Error::unsupported("this book keeps no encoded pages"))
    }

    /// The bytes of `page` for the browser reader, when its decoded `pixels`
    /// are at hand. Books that convert pages for the browser convert these
    /// instead of drawing the page again; the default ignores them.
    fn encoded_from(&self, page: usize, pixels: &Pixels, cancel: &Cancel) -> Result<Encoded> {
        let _ = pixels;
        self.encoded(page, cancel)
    }

    /// Stops converters and releases the file. Later page requests fail.
    fn close(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(title: &str, page: usize, depth: usize) -> Chapter {
        Chapter { title: title.into(), page, depth }
    }

    #[test]
    fn the_nearest_preceding_chapter_holds_a_page() {
        let chapters = [chapter("one", 0, 0), chapter("two", 10, 0), chapter("two-a", 10, 1), chapter("three", 20, 0)];
        assert_eq!(current_chapter(&chapters, 5), Some(0));
        assert_eq!(current_chapter(&chapters, 10), Some(2), "a subsection wins over its parent");
        assert_eq!(current_chapter(&chapters, 25), Some(3));
    }

    #[test]
    fn outline_order_does_not_matter() {
        let chapters = [chapter("late", 30, 0), chapter("early", 5, 0)];
        assert_eq!(current_chapter(&chapters, 10), Some(1));
        assert_eq!(current_chapter(&chapters, 2), None);
    }
}
