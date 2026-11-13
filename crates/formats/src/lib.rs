//! Book formats: comic archives (ZIP and RAR), and PDF and DJVU pages that
//! poppler and djvulibre draw. `open` tells formats apart by their signature,
//! not the file extension, and names text books (EPUB, FB2, DOCX, DOC) for
//! the text crate to read.

mod comic;
mod rendered;

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use yoyomi_core::{Book, Error, Result};

/// A text book format, which the text crate reads.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextFormat {
    Epub,
    Docx,
    /// An FB2 document as plain XML.
    Fb2,
    /// An FB2 document inside a ZIP archive, at `entry`.
    Fb2Zip {
        entry: String,
    },
    /// A Word 97-2003 document, which antiword reads.
    Doc,
}

/// What a file turned out to hold.
#[non_exhaustive]
pub enum Opened {
    /// A book this crate reads.
    Book(Arc<dyn Book>),
    /// A text book for the text crate, at the same path.
    Text(TextFormat),
}

impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Opened::Book(book) => f.debug_tuple("Book").field(&book.title()).field(&book.len()).finish(),
            Opened::Text(format) => f.debug_tuple("Text").field(format).finish(),
        }
    }
}

/// The OLE2 compound file signature of Word 97-2003 documents.
const OLE2: &[u8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";

/// Opens the book at `path`. PDF, DJVU and DOC go by their signatures, then
/// ZIP and RAR archives, then FB2 as XML, then a PDF signature behind leading
/// junk.
pub fn open(path: &Path) -> Result<Opened> {
    let path = std::path::absolute(path)?;
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let head = read_head(&path).map_err(|e| Error::Io(io::Error::new(e.kind(), format!("open {name}: {e}"))))?;
    if head.starts_with(b"%PDF-") {
        return rendered::open(&path, rendered::Tool::Pdf).map(Opened::Book);
    }
    if head.starts_with(b"AT&TFORM") {
        return rendered::open(&path, rendered::Tool::Djvu).map(Opened::Book);
    }
    if head.starts_with(OLE2) {
        return Ok(Opened::Text(TextFormat::Doc));
    }
    if let Some(opened) = comic::open(&path)? {
        return Ok(opened);
    }
    if head.starts_with(b"PK\x03\x04") {
        return Err(Error::Format(format!("{name}: broken ZIP archive")));
    }
    // The FB2 reader goes first; a PDF behind XML-looking junk opens through
    // `open_pdf` once it fails.
    if looks_like_xml(&head) {
        return Ok(Opened::Text(TextFormat::Fb2));
    }
    if head.windows(5).any(|w| w == b"%PDF-") {
        return rendered::open(&path, rendered::Tool::Pdf).map(Opened::Book);
    }
    Err(Error::Format(format!("{name}: unsupported format")))
}

/// Opens the file at `path` as a PDF whatever its first bytes: a PDF
/// behind junk that looks like XML, once the FB2 reader refused the file.
/// As `open`, the tools get an absolute path, so a name with a leading dash
/// is no option to them.
pub fn open_pdf(path: &Path) -> Result<Arc<dyn Book>> {
    rendered::open(&std::path::absolute(path)?, rendered::Tool::Pdf)
}

/// The first KiB of a file, which is how far a PDF header may sit behind
/// leading junk.
fn read_head(path: &Path) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    File::open(path)?.take(1024).read_to_end(&mut head)?;
    Ok(head)
}

/// Whether `head` starts a UTF-8 or UTF-16 document that opens with a tag.
fn looks_like_xml(head: &[u8]) -> bool {
    if head.starts_with(b"\xff\xfe") || head.starts_with(b"\xfe\xff") {
        return true;
    }
    let text = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    text.iter().copied().find(|b| !b" \t\r\n".contains(b)) == Some(b'<')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_needs_a_leading_tag() {
        assert!(looks_like_xml(b"<?xml version=\"1.0\"?><FictionBook/>"));
        assert!(looks_like_xml(b"\xef\xbb\xbf \n<FictionBook/>"));
        assert!(looks_like_xml(b"\xff\xfe<\x00"));
        assert!(!looks_like_xml(b"plain text <b>"));
        assert!(!looks_like_xml(b""));
    }

    #[test]
    fn a_missing_file_names_itself() {
        let error = open(Path::new("/nonexistent/book.cbz")).unwrap_err();
        assert!(error.to_string().contains("book.cbz"), "{error}");
    }

    #[test]
    fn doc_files_go_to_the_text_crate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.doc");
        let mut bytes = OLE2.to_vec();
        bytes.extend_from_slice(&[0; 64]);
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(open(&path), Ok(Opened::Text(TextFormat::Doc))));
    }
}
