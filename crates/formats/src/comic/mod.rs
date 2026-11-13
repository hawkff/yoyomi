//! ZIP and RAR comic archives.

mod chapters;
mod natsort;
mod rar;

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, MutexGuard};
use yoyomi_core::{Book, Cancel, Chapter, Encoded, Error, MAX_PAGE_BYTES, PageKind, Pixels, Result, check_size, decode, decoded_size, image};
use zip::ZipArchive;

use crate::rendered::encoded::png;
use crate::{Opened, TextFormat};

const IMAGE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "gif", "webp", "bmp"];

/// The most bytes ComicInfo.xml may take.
const MAX_METADATA_BYTES: usize = 1 << 20;

/// How often a waiting request looks at its `Cancel`, which has no way to
/// wake it.
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// The read buffer under a ZIP archive. On a share without a page cache
/// every read that reaches the file is a network round trip, and the zip
/// crate reads the central directory one field at a time. Through 1 MiB the
/// directory of a large archive arrives in one or two round trips, and a
/// page in about one per MiB.
const ZIP_BUFFER_BYTES: usize = 1 << 20;

/// Reads the directory of the ZIP archive in `reader` through the buffer.
fn zip_archive<R: Read + Seek>(reader: R) -> zip::result::ZipResult<ZipArchive<BufReader<R>>> {
    ZipArchive::new(BufReader::with_capacity(ZIP_BUFFER_BYTES, reader))
}

/// Locks `mutex`, giving up once `cancel` fires, so a cancelled request
/// never waits out another request's decode.
fn lock_or_cancel<'a, T>(mutex: &'a Mutex<T>, cancel: &Cancel) -> Result<MutexGuard<'a, T>> {
    loop {
        cancel.check()?;
        if let Some(guard) = mutex.try_lock_for(CANCEL_POLL) {
            return Ok(guard);
        }
    }
}

/// A file inside an archive.
#[derive(Clone, Debug)]
struct Entry {
    /// The path inside the archive, with `/` between folders.
    name: String,
    /// The entry's position in the archive's own listing.
    index: usize,
}

enum Archive {
    /// `None` once the book closes.
    Zip(Mutex<Option<ZipArchive<BufReader<File>>>>),
    Rar(rar::Rar),
}

/// Opens `path` as a ZIP or RAR archive, told apart by signature. Comic
/// archives come back as books; EPUB, DOCX and zipped FB2 come back as text
/// formats. Returns `None` when the file is neither ZIP nor RAR, so `open`
/// can try other formats.
pub(crate) fn open(path: &Path) -> Result<Option<Opened>> {
    let file_name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let (archive, entries) = match zip_archive(File::open(path)?) {
        Ok(zip) => {
            let mut entries = Vec::new();
            for (index, name) in zip.file_names().enumerate() {
                let name = name.map_err(|e| Error::Format(format!("open {file_name}: {e}")))?;
                if !name.ends_with('/') {
                    entries.push(Entry { name: name.into_owned(), index });
                }
            }
            (Archive::Zip(Mutex::new(Some(zip))), entries)
        }
        Err(_) => match rar::Rar::open(path).map_err(|e| Error::Format(format!("open {file_name}: {e}")))? {
            Some((rar, entries)) => (Archive::Rar(rar), entries),
            None => return Ok(None),
        },
    };

    // The contents win over the extension: an EPUB container, then DOCX, then
    // a zipped FB2. A bare .epub extension still means EPUB.
    let zip = matches!(archive, Archive::Zip(_));
    let has = |name: &str| entries.iter().any(|entry| entry.name == name);
    let (container, docx) = (has("META-INF/container.xml"), has("word/document.xml"));
    let fb2 = entries.iter().find(|entry| !is_hidden(&entry.name) && extension(&entry.name).eq_ignore_ascii_case("fb2"));
    if container || !docx && fb2.is_none() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("epub")) {
        if !zip {
            return Err(Error::Format("EPUB requires a ZIP container".into()));
        }
        return Ok(Some(Opened::Text(TextFormat::Epub)));
    }
    if zip && docx {
        return Ok(Some(Opened::Text(TextFormat::Docx)));
    }
    if zip && let Some(fb2) = fb2 {
        return Ok(Some(Opened::Text(TextFormat::Fb2Zip { entry: fb2.name.clone() })));
    }

    let comic_info = entries.iter().find(|entry| base_name(&entry.name).eq_ignore_ascii_case("ComicInfo.xml")).cloned();
    let mut pages: Vec<Entry> = entries.into_iter().filter(|entry| is_page(&entry.name)).collect();
    if pages.is_empty() {
        return Err(Error::Format(format!("{file_name}: no images found")));
    }
    pages.sort_by(|a, b| natsort::natural_cmp(&a.name, &b.name));
    let title = path.file_stem().map_or_else(|| file_name.clone(), |stem| stem.to_string_lossy().into_owned());
    let comic = Comic { path: path.to_path_buf(), title, archive, pages, comic_info, closed: Cancel::new() };
    Ok(Some(Opened::Book(Arc::new(comic))))
}

/// Junk entries: AppleDouble forks, hidden files and `__MACOSX` folders.
fn is_hidden(name: &str) -> bool {
    name.contains("__MACOSX/") || base_name(name).starts_with('.')
}

fn is_page(name: &str) -> bool {
    !is_hidden(name) && IMAGE_EXTENSIONS.iter().any(|ext| extension(name).eq_ignore_ascii_case(ext))
}

fn base_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// The text after the last dot of the base name, or "".
fn extension(name: &str) -> &str {
    base_name(name).rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// A comic book: the image entries of an archive in natural order.
struct Comic {
    path: PathBuf,
    title: String,
    archive: Archive,
    pages: Vec<Entry>,
    comic_info: Option<Entry>,
    closed: Cancel,
}

impl Comic {
    /// The bytes of `entry`, failing past `limit` bytes.
    fn read(&self, entry: &Entry, limit: usize, cancel: &Cancel) -> Result<Vec<u8>> {
        cancel.check()?;
        self.closed.check()?;
        let data = match &self.archive {
            Archive::Zip(zip) => {
                let mut zip = lock_or_cancel(zip, cancel)?;
                let file = zip.as_mut().ok_or(Error::Cancelled)?.by_index(entry.index).map_err(|e| Error::Format(format!("{}: {e}", entry.name)))?;
                // shortcut: entries decompress one at a time under the lock;
                // give each read its own file handle if parallel reads matter.
                read_bounded(file, limit, cancel, &self.closed)?
            }
            Archive::Rar(rar) => rar.read(entry.index, &entry.name, cancel)?,
        };
        if data.len() > limit {
            return Err(Error::Limit(format!("{} exceeds {limit} bytes", entry.name)));
        }
        Ok(data)
    }

    fn page_bytes(&self, page: usize, cancel: &Cancel) -> Result<Vec<u8>> {
        let entry = self.pages.get(page).ok_or_else(|| Error::Format(format!("page {page} out of range")))?;
        self.read(entry, MAX_PAGE_BYTES, cancel)
    }
}

/// Reads `reader` to its end, checking both tokens between reads, and fails
/// once it passes `limit` bytes.
fn read_bounded(mut reader: impl Read, limit: usize, cancel: &Cancel, closed: &Cancel) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut chunk = vec![0; 64 << 10];
    loop {
        cancel.check()?;
        closed.check()?;
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Ok(data);
        }
        if data.len() + n > limit {
            return Err(Error::Limit(format!("data exceeds {limit} bytes")));
        }
        data.extend_from_slice(&chunk[..n]);
    }
}

impl Book for Comic {
    fn path(&self) -> &Path {
        &self.path
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn len(&self) -> usize {
        self.pages.len()
    }

    fn kind(&self, _page: usize) -> PageKind {
        PageKind::Image
    }

    fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
        let bytes = self.page_bytes(page, cancel)?;
        Ok(Arc::new(decode(&bytes, cancel)?))
    }

    fn page_size(&self, page: usize, cancel: &Cancel) -> Result<(u32, u32)> {
        decoded_size(&self.page_bytes(page, cancel)?)
    }

    /// ComicInfo.xml bookmarks, or else one chapter per folder of pages.
    fn chapters(&self, cancel: &Cancel) -> Result<Vec<Chapter>> {
        if let Some(entry) = &self.comic_info {
            let bookmarks = chapters::comic_info(&self.read(entry, MAX_METADATA_BYTES, cancel)?, self.pages.len())?;
            if !bookmarks.is_empty() {
                return Ok(bookmarks);
            }
        }
        cancel.check()?;
        Ok(chapters::folders(self.pages.iter().map(|page| page.name.as_str())))
    }

    fn encoded(&self, page: usize, cancel: &Cancel) -> Result<Encoded> {
        let bytes = self.page_bytes(page, cancel)?;
        let (width, height) = decoded_size(&bytes)?;
        check_size(width, height)?;
        let format = image::guess_format(&bytes)?;
        // Browsers cannot show PPM.
        if format == image::ImageFormat::Pnm {
            return png(&decode(&bytes, cancel)?);
        }
        Ok(Encoded { bytes: bytes.into(), mime: format.to_mime_type() })
    }

    fn close(&self) {
        self.closed.cancel();
        match &self.archive {
            Archive::Zip(zip) => *zip.lock() = None,
            Archive::Rar(rar) => rar.close(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::{self, Cursor, SeekFrom, Write};
    use std::rc::Rc;
    use std::thread;
    use std::time::Instant;

    use yoyomi_core::image::{ImageFormat, RgbaImage};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(name)
    }

    fn open_book(path: &Path) -> Arc<dyn Book> {
        match open(path).unwrap() {
            Some(Opened::Book(book)) => book,
            other => panic!("{other:?}"),
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::new(width, height).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn write_zip(dir: &Path, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join(name);
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        for (entry, data) in entries {
            zip.start_file(*entry, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    /// Checks that `read`, stuck behind a decode that holds the archive, gives
    /// up soon after its token is cancelled.
    pub(super) fn assert_cancel_unblocks(read: impl FnOnce(&Cancel) -> Result<Vec<u8>> + Send + 'static) {
        let cancel = Cancel::new();
        let token = cancel.clone();
        let reader = thread::spawn(move || read(&token));
        thread::sleep(Duration::from_millis(20));
        assert!(!reader.is_finished(), "the read did not wait for the decode");
        cancel.cancel();
        let cancelled = Instant::now();
        while !reader.is_finished() && cancelled.elapsed() < Duration::from_secs(1) {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(cancelled.elapsed() < Duration::from_millis(50), "the read took {:?} to notice its cancel", cancelled.elapsed());
        assert!(reader.join().unwrap().unwrap_err().is_cancelled());
    }

    #[test]
    fn cancelled_zip_reads_do_not_wait_for_another_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_zip(dir.path(), "slow.cbz", &[("page.png", &png(2, 2))]);
        let zip = zip_archive(File::open(&path).unwrap()).unwrap();
        let page = Entry { name: "page.png".into(), index: 0 };
        let comic = Arc::new(Comic {
            path,
            title: "slow".into(),
            archive: Archive::Zip(Mutex::new(Some(zip))),
            pages: vec![page],
            comic_info: None,
            closed: Cancel::new(),
        });
        let Archive::Zip(zip) = &comic.archive else { unreachable!() };
        let decode = zip.lock(); // another page in the middle of a slow decode
        let reader = comic.clone();
        assert_cancel_unblocks(move |cancel| reader.page_bytes(0, cancel));
        drop(decode);
        assert!(!comic.page_bytes(0, &Cancel::new()).unwrap().is_empty());
    }

    /// A file that counts the reads reaching it, each one a round trip on a
    /// share.
    struct CountingFile {
        file: File,
        reads: Rc<Cell<usize>>,
    }

    impl Read for CountingFile {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reads.set(self.reads.get() + 1);
            self.file.read(buf)
        }
    }

    impl Seek for CountingFile {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.file.seek(pos)
        }
    }

    #[test]
    fn zip_archives_reach_the_file_in_large_reads() {
        const MIB: usize = 1 << 20;
        // Incompressible bytes, so the deflated page takes as many bytes as the stored one.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let noise: Vec<u8> = (0..3 * MIB)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.cbz");
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        for i in 0..500 {
            zip.start_file(format!("pages/{i:03}.png"), SimpleFileOptions::default()).unwrap();
            zip.write_all(&[i as u8; 100]).unwrap();
        }
        for (name, method) in [("stored.png", CompressionMethod::Stored), ("deflated.png", CompressionMethod::Deflated)] {
            zip.start_file(name, SimpleFileOptions::default().compression_method(method)).unwrap();
            zip.write_all(&noise).unwrap();
        }
        zip.finish().unwrap();

        let reads = Rc::new(Cell::new(0));
        let mut zip = zip_archive(CountingFile { file: File::open(&path).unwrap(), reads: reads.clone() }).unwrap();
        assert_eq!(zip.len(), 502);
        assert!(reads.get() <= 4, "opening took {} reads", reads.get());
        let (cancel, closed) = (Cancel::new(), Cancel::new());
        for (index, data, most) in [(250, &[250u8; 100][..], 2), (500, &noise[..], 4), (501, &noise[..], 5)] {
            reads.set(0);
            assert_eq!(read_bounded(zip.by_index(index).unwrap(), MAX_PAGE_BYTES, &cancel, &closed).unwrap(), data, "entry {index}");
            assert!(reads.get() <= most, "entry {index} took {} reads", reads.get());
        }
    }

    #[test]
    fn a_ppm_page_reaches_the_browser_as_png() {
        let dir = tempfile::tempdir().unwrap();
        let book = open_book(&write_zip(dir.path(), "ppm.cbz", &[("page.png", b"P6\n1 1\n255\n\x0a\x14\x1e")]));
        let encoded = book.encoded(0, &Cancel::new()).unwrap();
        assert_eq!(encoded.mime, "image/png");
        assert_eq!(image::guess_format(&encoded.bytes).unwrap(), ImageFormat::Png);
        assert_eq!(decode(&encoded.bytes, &Cancel::new()).unwrap().get_pixel(0, 0).0, [10, 20, 30, 255]);
    }

    #[test]
    fn hidden_entries_are_junk() {
        assert!(is_hidden("__MACOSX/ch1/._p1.png"));
        assert!(is_hidden("ch1/.hidden.png"));
        assert!(is_hidden(".DS_Store"));
        assert!(!is_hidden("ch1/p1.png"));
        assert!(is_page("ch1/P1.JPEG") && !is_page("notes.txt") && !is_page("ch1/.p1.png"));
    }

    #[test]
    fn nested_zip_pages_come_in_natural_order() {
        let book = open_book(&fixture("nested.cbz"));
        assert_eq!(book.title(), "nested");
        assert!(book.path().is_absolute());
        assert_eq!(book.len(), 5);
        let cancel = Cancel::new();
        for i in 0..5u8 {
            let pixels = book.page(i.into(), &cancel).unwrap();
            assert_eq!(pixels.get_pixel(0, 0).0, [i * 40, 255 - i * 40, 128, 255], "page {i}");
        }
        assert_eq!(book.kind(0), PageKind::Image);
        assert_eq!(book.page_size(4, &cancel).unwrap(), (30, 40));
        assert_eq!(book.encoded(0, &cancel).unwrap().mime, "image/png");
        let chapters = book.chapters(&cancel).unwrap();
        let chapters: Vec<(&str, usize)> = chapters.iter().map(|c| (c.title.as_str(), c.page)).collect();
        assert_eq!(chapters, [("Opening", 0), ("Second act", 3)]);
        assert!(book.page(5, &cancel).is_err());
        book.close();
        assert!(book.page(0, &cancel).unwrap_err().is_cancelled());
    }

    #[test]
    fn webtoon_page_sizes_come_from_headers() {
        let book = open_book(&fixture("webtoon.cbz"));
        let sizes: Vec<(u32, u32)> = (0..book.len()).map(|i| book.page_size(i, &Cancel::new()).unwrap()).collect();
        assert_eq!(sizes, [(120, 500), (120, 700), (300, 400), (60, 300)]);
    }

    #[test]
    fn solid_rar_pages_decode_in_any_order() {
        let book = open_book(&fixture("solid.cbr"));
        assert_eq!(book.len(), 3);
        // Each page is a 4-pixel-wide strip, 2+i rows high, in red, green and blue.
        let colors = [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]];
        for i in [2, 0, 1, 2, 1] {
            let pixels = book.page(i, &Cancel::new()).unwrap();
            assert_eq!(pixels.dimensions(), (4, 2 + i as u32), "page {i}");
            assert_eq!(pixels.get_pixel(1, 1).0, colors[i], "page {i}");
        }
        book.close();
        assert!(book.page(2, &Cancel::new()).unwrap_err().is_cancelled());
    }

    #[test]
    fn solid_bmp_rar_pages_decode() {
        let book = open_book(&fixture("solid-bmp.cbr"));
        for i in 0..3 {
            assert_eq!(book.page(i, &Cancel::new()).unwrap().dimensions(), (64, 64));
        }
    }

    #[test]
    fn archives_without_pages_name_themselves() {
        let error = open(&fixture("split.part1.rar")).unwrap_err();
        assert_eq!(error.to_string(), "split.part1.rar: no images found");
        let dir = tempfile::tempdir().unwrap();
        let path = write_zip(dir.path(), "notes.cbz", &[("notes.txt", b"text"), ("__MACOSX/._p.png", b"junk")]);
        assert_eq!(open(&path).unwrap_err().to_string(), "notes.cbz: no images found");
    }

    #[test]
    fn other_files_are_not_archives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "plain text").unwrap();
        assert!(open(&path).unwrap().is_none());
        // A download cut short keeps the ZIP signature but loses the directory.
        let page = png(2, 2);
        let zip = std::fs::read(write_zip(dir.path(), "full.cbz", &[("page.png", &page)])).unwrap();
        let path = dir.path().join("partial.cbz");
        std::fs::write(&path, &zip[..zip.len() / 2]).unwrap();
        assert!(open(&path).unwrap().is_none());
        let error = crate::open(&path).unwrap_err();
        assert!(error.to_string().contains("partial.cbz"), "{error}");
    }

    #[test]
    fn zip_contents_name_text_formats() {
        let dir = tempfile::tempdir().unwrap();
        let page = png(2, 2);
        let format = |name: &str, entries: &[(&str, &[u8])]| match open(&write_zip(dir.path(), name, entries)).unwrap() {
            Some(Opened::Text(format)) => format,
            other => panic!("{name}: {other:?}"),
        };
        assert_eq!(format("a.zip", &[("META-INF/container.xml", b""), ("word/document.xml", b"")]), TextFormat::Epub);
        assert_eq!(format("b.zip", &[("word/document.xml", b""), ("book.fb2", b"")]), TextFormat::Docx);
        assert_eq!(format("c.cbz", &[("page.png", &page), ("._book.fb2", b""), ("Book.FB2", b"")]), TextFormat::Fb2Zip { entry: "Book.FB2".into() });
        assert_eq!(format("d.epub", &[("page.png", &page)]), TextFormat::Epub);
    }

    #[test]
    fn epub_needs_a_zip_container() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.epub");
        std::fs::copy(fixture("solid.cbr"), &path).unwrap();
        assert_eq!(open(&path).unwrap_err().to_string(), "EPUB requires a ZIP container");
    }

    #[test]
    fn pages_reject_oversized_dimensions() {
        // BMP dimensions need no pixel payload for the header.
        let mut bmp = vec![0; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&50000u32.to_le_bytes());
        bmp[22..26].copy_from_slice(&50000u32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        let dir = tempfile::tempdir().unwrap();
        let book = open_book(&write_zip(dir.path(), "huge.cbz", &[("page.bmp", &bmp)]));
        for error in [book.page(0, &Cancel::new()).unwrap_err(), book.encoded(0, &Cancel::new()).unwrap_err()] {
            assert!(error.to_string().contains("decoded pixels"), "{error}");
        }
    }

    #[test]
    fn reads_stop_at_their_limit_and_their_tokens() {
        let (cancel, closed) = (Cancel::new(), Cancel::new());
        assert!(matches!(read_bounded(io::repeat(0), 100, &cancel, &closed), Err(Error::Limit(_))));
        assert_eq!(read_bounded(&[1u8; 4][..], 4, &cancel, &closed).unwrap(), [1; 4]);
        closed.cancel();
        assert!(read_bounded(io::repeat(0), 100, &cancel, &closed).unwrap_err().is_cancelled());
        let book = open_book(&fixture("nested.cbz"));
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(book.page(0, &cancel).unwrap_err().is_cancelled());
    }

    #[test]
    fn chapters_fall_back_to_folders() {
        let dir = tempfile::tempdir().unwrap();
        let page = png(2, 2);
        let book =
            open_book(&write_zip(dir.path(), "folders.cbz", &[("First/1.png", &page), ("Last/2.png", &page), ("ComicInfo.xml", b"<ComicInfo/>")]));
        let chapters = book.chapters(&Cancel::new()).unwrap();
        assert_eq!(chapters.iter().map(|c| (c.title.as_str(), c.page)).collect::<Vec<_>>(), [("First", 0), ("Last", 1)]);
        let huge = vec![b' '; MAX_METADATA_BYTES + 1];
        let book = open_book(&write_zip(dir.path(), "metadata.cbz", &[("page.png", &page), ("ComicInfo.xml", &huge)]));
        assert!(matches!(book.chapters(&Cancel::new()), Err(Error::Limit(_))));
    }
}
