//! Text books: EPUB, FB2, DOCX and DOC, laid out into fixed pages with
//! embedded and system fonts. `formats::open` names the format; this crate
//! opens the book.
//!
//! Pages are 900 x 1200 pixels. Text pages rasterize on demand from the
//! glyphs the layout placed, and pictures keep pages of their own.

mod book;
mod doc;
mod docx;
mod epub;
mod fb2;
mod fonts;
mod layout;
mod path;
mod xml;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;

use yoyomi_core::{Book, Error, Result};
use yoyomi_formats::TextFormat;

/// Opens the text book at `path`, which `formats::open` found to hold
/// `format`, and lays it out.
pub fn open(path: &Path, format: &TextFormat) -> Result<Arc<dyn Book>> {
    let path = std::path::absolute(path)?;
    let book = match format {
        TextFormat::Epub => epub::open(&path)?,
        TextFormat::Docx => docx::open(&path)?,
        TextFormat::Fb2 => fb2::open_file(&path)?,
        TextFormat::Fb2Zip { entry } => fb2::open_zip(&path, entry)?,
        TextFormat::Doc => doc::open(&path)?,
        other => return Err(Error::unsupported(format!("{other:?} books are not supported"))),
    };
    Ok(Arc::new(book))
}
