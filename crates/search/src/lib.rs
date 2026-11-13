//! Search through a book: its text pages, PDF and DJVU text layers, and
//! tesseract OCR for the other pages, in the background and cancellable.

mod ocr;
mod search;

pub use ocr::{ocr_lang, recognize, tesseract_available};
pub use search::{Progress, Search, may_have_text, searchable};
