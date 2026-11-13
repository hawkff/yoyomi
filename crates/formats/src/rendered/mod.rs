//! PDF and DJVU books whose pages poppler and djvulibre draw on demand.

pub(crate) mod encoded;
mod outline;
mod text;
mod tool;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};
use yoyomi_core::{Book, Cancel, Chapter, Encoded, Error, MAX_PAGE_BYTES, PageKind, Pixels, Result, TextLayer, decode};

use self::encoded::EncodedPages;
use self::text::TextLayers;
use self::tool::{POLL, run};

/// The longest side of a drawn page, in pixels.
const PAGE_SIDE: u32 = 2000;

/// The most pages a document may report.
const MAX_PAGES: usize = 100_000;

/// The most bytes pdfinfo and djvused may print for the sizes of every page:
/// two lines of up to 64 bytes per page.
const MAX_SIZES_BYTES: usize = MAX_PAGES * 128;

/// The tools that read a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tool {
    /// pdfinfo counts the pages and pdftoppm draws them.
    Pdf,
    /// djvused counts the pages and ddjvu draws them.
    Djvu,
}

/// Opens a PDF or DJVU file. It counts the pages now and draws each one on
/// access, scaled to fit 2000 x 2000 pixels.
pub(crate) fn open(path: &Path, tool: Tool) -> Result<Arc<dyn Book>> {
    let closed = Cancel::new();
    let (counter, out) = match tool {
        Tool::Pdf => ("pdfinfo", run(Command::new("pdfinfo").arg(path), MAX_PAGE_BYTES, &[&closed])?),
        Tool::Djvu => ("djvused", run(Command::new("djvused").args(["-e", "n"]).arg(path), MAX_PAGE_BYTES, &[&closed])?),
    };
    let out = String::from_utf8_lossy(&out);
    let count = match tool {
        Tool::Pdf => out.lines().filter_map(|line| line.strip_prefix("Pages:")).next_back().and_then(|n| n.trim().parse().ok()),
        Tool::Djvu => out.trim().parse().ok(),
    }
    .unwrap_or(0);
    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
    if !(1..=MAX_PAGES).contains(&count) {
        return Err(Error::Format(format!("{name}: {counter} reported {count} pages")));
    }
    Ok(Arc::new(RenderedBook {
        path: path.to_owned(),
        title: path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
        tool,
        pages: count,
        closed,
        sizes: Mutex::new(None),
        chapters: Mutex::new(None),
        text: TextLayers::default(),
        search_pages: Mutex::new(None),
        encoded_pages: Mutex::default(),
    }))
}

/// A document whose pages a tool draws as binary PPM on each request.
struct RenderedBook {
    path: PathBuf,
    title: String,
    tool: Tool,
    pages: usize,
    /// Cancelled by `close`, which stops every running tool.
    closed: Cancel,
    /// The pixel size of every page, read once.
    sizes: Mutex<Option<Vec<(u32, u32)>>>,
    /// The outline, read once. Cancelled reads leave it empty for the next try.
    chapters: Mutex<Option<Result<Vec<Chapter>>>>,
    text: TextLayers,
    /// The search text of every PDF page, read once; empty when the run
    /// failed or did not split into the pages.
    search_pages: Mutex<Option<Vec<String>>>,
    /// Pages converted to PNG for the browser reader.
    encoded_pages: Mutex<EncodedPages>,
}

impl RenderedBook {
    fn check_page(&self, page: usize) -> Result<()> {
        if page < self.pages { Ok(()) } else { Err(Error::Format(format!("{}: no page {} of {}", self.title, page + 1, self.pages))) }
    }

    /// The page as a PNG, converted once and cached, from `pixels` or else
    /// from the page drawn again. Requests for any page wait for a running
    /// conversion, so a page converts only once.
    fn png(&self, page: usize, pixels: Option<&Pixels>, cancel: &Cancel) -> Result<Encoded> {
        self.check_page(page)?;
        let mut pages = lock(&self.encoded_pages, &[cancel, &self.closed])?;
        if let Some(cached) = pages.get(page) {
            return Ok(cached);
        }
        let pixels = match pixels {
            Some(pixels) => pixels.clone(),
            None => self.page(page, cancel)?,
        };
        let png = encoded::png(&pixels)?;
        pages.insert(page, png.clone());
        Ok(png)
    }

    /// `search_text` with `pdftotext` as the program that reads the
    /// document. A run that failed or did not split into the pages leaves
    /// every page to its text layer.
    fn search_text_with(&self, page: usize, cancel: &Cancel, mut pdftotext: Command) -> Result<Option<String>> {
        self.check_page(page)?;
        if self.tool != Tool::Pdf {
            return Ok(None);
        }
        let mut pages = lock(&self.search_pages, &[cancel, &self.closed])?;
        if pages.is_none() {
            pdftotext.args(["-q", "-enc", "UTF-8"]).arg(&self.path).arg("-");
            match run(&mut pdftotext, MAX_PAGE_BYTES, &[&self.closed]) {
                Err(error) if error.is_cancelled() => return Err(error),
                out => *pages = Some(text::search_pages(&String::from_utf8_lossy(&out.unwrap_or_default()), self.pages)),
            }
        }
        Ok(pages.as_ref().and_then(|pages| pages.get(page)).cloned())
    }

    /// Asks pdfinfo or djvused for the pixel size of every page.
    fn read_page_sizes(&self, cancel: &Cancel) -> Result<Vec<(u32, u32)>> {
        let stops = [cancel, &self.closed];
        match self.tool {
            Tool::Pdf => {
                let mut command = Command::new("pdfinfo");
                command.args(["-f", "1", "-l", &self.pages.to_string()]).arg(&self.path);
                pdf_page_sizes(&String::from_utf8_lossy(&run(&mut command, MAX_SIZES_BYTES, &stops)?), self.pages)
            }
            Tool::Djvu => {
                let out = run(Command::new("djvused").args(["-e", "size"]).arg(&self.path), MAX_SIZES_BYTES, &stops)?;
                djvu_page_sizes(&String::from_utf8_lossy(&out), self.pages)
            }
        }
    }
}

impl Book for RenderedBook {
    fn path(&self) -> &Path {
        &self.path
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn len(&self) -> usize {
        self.pages
    }

    fn kind(&self, _page: usize) -> PageKind {
        PageKind::Rendered
    }

    fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
        self.check_page(page)?;
        let n = (page + 1).to_string();
        let mut command = match self.tool {
            Tool::Pdf => {
                let mut command = Command::new("pdftoppm");
                command.args(["-cropbox", "-scale-to", &PAGE_SIDE.to_string(), "-f", &n, "-l", &n, "-singlefile"]);
                command
            }
            Tool::Djvu => {
                let mut command = Command::new("ddjvu");
                command.args(["-format=ppm", &format!("-size={PAGE_SIDE}x{PAGE_SIDE}"), &format!("-page={n}")]);
                command
            }
        };
        let ppm = run(command.arg(&self.path), MAX_PAGE_BYTES, &[cancel, &self.closed])?;
        Ok(Arc::new(decode(&ppm, cancel)?))
    }

    /// The text of a PDF page from one pdftotext run over the whole
    /// document, kept while the book is open. Only closing the book stops
    /// that run, so a later search uses what a cancelled one started.
    fn search_text(&self, page: usize, cancel: &Cancel) -> Result<Option<String>> {
        self.search_text_with(page, cancel, Command::new("pdftotext"))
    }

    fn page_size(&self, page: usize, cancel: &Cancel) -> Result<(u32, u32)> {
        self.check_page(page)?;
        let mut cached = lock(&self.sizes, &[cancel, &self.closed])?;
        let sizes = match cached.take() {
            Some(sizes) => sizes,
            None => self.read_page_sizes(cancel)?,
        };
        let size = sizes[page];
        *cached = Some(sizes);
        Ok(size)
    }

    fn chapters(&self, cancel: &Cancel) -> Result<Vec<Chapter>> {
        let mut chapters = lock(&self.chapters, &[cancel, &self.closed])?;
        if let Some(chapters) = &*chapters {
            return chapters.clone();
        }
        let read = outline::chapters(&self.path, self.tool, self.pages, &[cancel, &self.closed]);
        if !matches!(&read, Err(error) if error.is_cancelled()) {
            *chapters = Some(read.clone());
        }
        read
    }

    fn text_layer(&self, page: usize, cancel: &Cancel) -> Result<Option<Arc<TextLayer>>> {
        self.check_page(page)?;
        let layer = self.text.layer(&self.path, self.tool, self.pages, page, &[cancel, &self.closed])?;
        Ok(Some(layer).filter(|layer| !layer.lines.is_empty()))
    }

    fn encoded(&self, page: usize, cancel: &Cancel) -> Result<Encoded> {
        self.png(page, None, cancel)
    }

    fn encoded_from(&self, page: usize, pixels: &Pixels, cancel: &Cancel) -> Result<Encoded> {
        self.png(page, Some(pixels), cancel)
    }

    fn close(&self) {
        self.closed.cancel();
    }
}

/// Locks `mutex`, which a running tool may hold for a while, unless one of
/// `stops` fires first.
fn lock<'a, T>(mutex: &'a Mutex<T>, stops: &[&Cancel]) -> Result<MutexGuard<'a, T>> {
    loop {
        stops.iter().try_for_each(|stop| stop.check())?;
        if let Some(guard) = mutex.try_lock_for(POLL) {
            return Ok(guard);
        }
    }
}

/// Reads the size of every page from `pdfinfo -f 1 -l <pages>`, which prints
/// `Page <n> size: <w> x <h> pts` and `Page <n> rot: <degrees>`, and converts
/// it to the pixels `pdftoppm -cropbox -scale-to 2000` draws.
fn pdf_page_sizes(info: &str, pages: usize) -> Result<Vec<(u32, u32)>> {
    let mut points = vec![None; pages];
    let mut turned = vec![false; pages];
    for line in info.lines() {
        let Some(rest) = line.strip_prefix("Page") else { continue };
        let Some((n, value)) = rest.trim_start().split_once(char::is_whitespace) else { continue };
        let Some(i) = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|&i| i < pages) else { continue };
        let value = value.trim_start();
        if let Some(size) = value.strip_prefix("size:") {
            let mut numbers = size.split_whitespace();
            let (Some(w), Some("x"), Some(h)) = (numbers.next(), numbers.next(), numbers.next()) else { continue };
            points[i] = w.parse::<f64>().ok().zip(h.parse::<f64>().ok());
        } else if let Some(rotation) = value.strip_prefix("rot:") {
            turned[i] = rotation.trim().parse::<i32>().is_ok_and(|degrees| degrees.rem_euclid(180) == 90);
        }
    }
    points
        .into_iter()
        .zip(turned)
        .enumerate()
        .map(|(i, (size, turned))| {
            let (w, h) = size.ok_or_else(|| Error::Format(format!("pdfinfo gave no size for page {}", i + 1)))?;
            let (w, h) = pdftoppm_size(w, h);
            Ok(if turned { (h, w) } else { (w, h) })
        })
        .collect()
}

/// The pixel size pdftoppm's `-scale-to` gives a page of `w` x `h` points:
/// the longer side becomes `PAGE_SIDE`, and both sides round up.
fn pdftoppm_size(w: f64, h: f64) -> (u32, u32) {
    let side = f64::from(PAGE_SIDE);
    // pdftoppm computes a resolution first; the same steps keep its rounding.
    let (w, h) = if w > h { (side, h * (72.0 * side / w / 72.0)) } else { (w * (72.0 * side / h / 72.0), side) };
    (w.ceil() as u32, h.ceil() as u32)
}

/// Reads the size of every page from `djvused -e size`, which prints
/// `width=<w> height=<h>` and, for turned pages, ` rotation=<quarter turns>`
/// per page, and converts it to the pixels `ddjvu -size=2000x2000` draws.
fn djvu_page_sizes(listing: &str, pages: usize) -> Result<Vec<(u32, u32)>> {
    let sizes = listing
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let field = |key: &str| line.split_whitespace().find_map(|pair| pair.strip_prefix(key)?.parse::<u32>().ok());
            let (w, h) = field("width=").zip(field("height=")).ok_or_else(|| Error::Format(format!("djvused printed {line:?} as a page size")))?;
            let (w, h) = if field("rotation=").unwrap_or(0) % 2 == 1 { (h, w) } else { (w, h) };
            Ok(ddjvu_size(w, h))
        })
        .collect::<Result<Vec<_>>>()?;
    if sizes.len() != pages {
        return Err(Error::Format(format!("djvused gave {} page sizes for {pages} pages", sizes.len())));
    }
    Ok(sizes)
}

/// The pixel size ddjvu's `-size=2000x2000` gives a page of `w` x `h` pixels:
/// it fits the box and keeps the aspect ratio, and the shorter side rounds
/// down.
fn ddjvu_size(w: u32, h: u32) -> (u32, u32) {
    let side = f64::from(PAGE_SIDE);
    let (dw, dh) = (f64::from(w) / side, f64::from(h) / side);
    if dw > dh { (PAGE_SIDE, (f64::from(h) / dw) as u32) } else { ((f64::from(w) / dh) as u32, PAGE_SIDE) }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;

    use super::*;
    use crate::Opened;

    /// Whether every tool is on PATH; says which one is missing otherwise.
    fn have(tools: &[&str]) -> bool {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let missing = tools.iter().find(|tool| !std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()));
        if let Some(tool) = missing {
            eprintln!("skipped: {tool} is not installed");
        }
        missing.is_none()
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn open_book(path: &Path) -> Arc<dyn Book> {
        match crate::open(path).unwrap() {
            Opened::Book(book) => book,
            Opened::Text(format) => panic!("{} opened as {format:?}", path.display()),
        }
    }

    fn tool(program: &str, args: &[&str]) {
        let out = Command::new(program).args(args).output().unwrap();
        assert!(out.status.success(), "{program}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A PDF whose pages have the given objects after `/Type/Page`.
    fn pdf(pages: &[&str]) -> Vec<u8> {
        let kids: Vec<String> = (0..pages.len()).map(|i| format!("{} 0 R", i + 3)).collect();
        let mut pdf = format!(
            "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[{}]/Count {}>>endobj\n",
            kids.join(" "),
            pages.len()
        );
        for (i, page) in pages.iter().enumerate() {
            pdf += &format!("{} 0 obj<</Type/Page/Parent 2 0 R{page}>>endobj\n", i + 3);
        }
        (pdf + "trailer<</Root 1 0 R>>\n").into_bytes()
    }

    /// A DJVU page of `w` x `h` red pixels, turned by `rotation` quarter turns.
    fn djvu_page(dir: &Path, name: &str, (w, h): (u32, u32), rotation: u32) -> PathBuf {
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        ppm.extend([255, 0, 0].repeat((w * h) as usize));
        let source = write(dir, &format!("{name}.ppm"), &ppm);
        let page = dir.join(format!("{name}.djvu"));
        tool("c44", &[source.to_str().unwrap(), page.to_str().unwrap()]);
        if rotation != 0 {
            tool("djvused", &["-e", &format!("set-rotation {rotation}; save"), page.to_str().unwrap()]);
        }
        page
    }

    #[test]
    fn open_pdf() {
        if !have(&["pdfinfo", "pdftoppm", "pdftohtml"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bytes = pdf(&["/MediaBox[0 0 200 100]/CropBox[0 0 100 100]", "/MediaBox[0 0 100 200]"]);
        let mut shifted = b"junk\n".repeat(100);
        shifted.extend(&bytes);
        assert_eq!(open(&write(dir.path(), "shifted.pdf", &shifted), Tool::Pdf).unwrap().len(), 2, "a PDF behind leading junk");
        let book = open_book(&write(dir.path(), "two.pdf", &bytes));
        assert_eq!((book.title(), book.len(), book.kind(0)), ("two", 2, PageKind::Rendered));
        assert!(book.can_invert(0));
        let cancel = Cancel::new();
        assert_eq!(book.chapters(&cancel).unwrap(), []);
        assert_eq!(book.page(1, &cancel).unwrap().dimensions(), (1000, 2000));
        assert_eq!(book.page(0, &cancel).unwrap().dimensions(), (2000, 2000), "pages keep their crop box");
        assert!(matches!(book.page(2, &cancel), Err(Error::Format(_))));
        assert!(matches!(book.page_size(2, &cancel), Err(Error::Format(_))));
    }

    #[test]
    fn a_pdf_with_a_megabyte_title_opens() {
        if !have(&["pdfinfo"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = pdf(&["/MediaBox[0 0 100 100]"]);
        bytes.truncate(bytes.len() - "trailer<</Root 1 0 R>>\n".len());
        bytes.extend(format!("4 0 obj<</Title({})>>endobj\ntrailer<</Root 1 0 R/Info 4 0 R>>\n", "A".repeat(1 << 20)).into_bytes());
        assert_eq!(open(&write(dir.path(), "titled.pdf", &bytes), Tool::Pdf).unwrap().len(), 1);
    }

    #[test]
    fn a_broken_pdf_names_pdfinfo() {
        if !have(&["pdfinfo"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let error = crate::open(&write(dir.path(), "broken.pdf", b"%PDF-1.4\nnothing here\n")).unwrap_err();
        assert!(matches!(&error, Error::Tool { tool, .. } if tool == "pdfinfo"), "{error}");
    }

    #[test]
    fn pdf_page_sizes_match_pdftoppm() {
        if !have(&["pdfinfo", "pdftoppm"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sizes.pdf",
            &pdf(&[
                "/MediaBox[0 0 200 100]/CropBox[0 0 100 100]",
                "/MediaBox[0 0 595.2756 841.8898]/Rotate 90",
                "/MediaBox[0 0 333.3 777.7]",
                "/MediaBox[0 0 612 792]/Rotate 270",
                "/MediaBox[0 0 1000 3]",
            ]),
        );
        let book = open_book(&path);
        let cancel = Cancel::new();
        for page in 0..book.len() {
            assert_eq!(book.page_size(page, &cancel).unwrap(), book.page(page, &cancel).unwrap().dimensions(), "page {}", page + 1);
        }
    }

    #[test]
    fn pdf_page_sizes_parse_pdfinfo() {
        let info = "Pages:           2\nPage    1 size:  100 x 100 pts\nPage    1 rot:   0\nPage    2 size:  595.276 x 841.89 pts (A4)\nPage    2 rot:   90\n";
        assert_eq!(pdf_page_sizes(info, 2).unwrap(), [(2000, 2000), (2000, 1415)]);
        assert!(pdf_page_sizes(info, 3).is_err(), "a page without a size");
    }

    #[test]
    fn open_djvu() {
        if !have(&["c44", "djvused", "ddjvu"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = djvu_page(dir.path(), "scan", (64, 32), 0);
        let book = open_book(&path);
        assert_eq!((book.title(), book.len(), book.kind(0)), ("scan", 1, PageKind::Rendered));
        assert!(book.can_invert(0));
        let page = book.page(0, &Cancel::new()).unwrap();
        assert_eq!(page.dimensions(), (2000, 1000));
        let [r, g, _, _] = page.get_pixel(1000, 500).0;
        assert!(r > 0x80 && g < 0x80, "the page lost its color");
    }

    #[test]
    fn djvu_page_sizes_match_ddjvu() {
        if !have(&["c44", "djvm", "djvused", "ddjvu"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pages: Vec<PathBuf> = [((1001, 3), 0), ((333, 777), 0), ((300, 123), 1), ((77, 1999), 3), ((1999, 2001), 2)]
            .into_iter()
            .enumerate()
            .map(|(i, (size, rotation))| djvu_page(dir.path(), &format!("p{i}"), size, rotation))
            .collect();
        let path = dir.path().join("book.djvu");
        let mut args = vec!["-c", path.to_str().unwrap()];
        args.extend(pages.iter().map(|page| page.to_str().unwrap()));
        tool("djvm", &args);
        let book = open_book(&path);
        let cancel = Cancel::new();
        assert_eq!(book.len(), 5);
        for page in 0..book.len() {
            assert_eq!(book.page_size(page, &cancel).unwrap(), book.page(page, &cancel).unwrap().dimensions(), "page {}", page + 1);
        }
    }

    #[test]
    fn djvu_page_sizes_parse_djvused() {
        let listing = "width=64 height=32\nwidth=3000 height=1234 rotation=1\n";
        assert_eq!(djvu_page_sizes(listing, 2).unwrap(), [(2000, 1000), (822, 2000)]);
        assert!(djvu_page_sizes(listing, 3).is_err());
        assert!(djvu_page_sizes("width=64\n", 1).is_err());
    }

    #[test]
    fn pdf_outline_destinations() {
        if !have(&["pdfinfo", "pdftohtml"]) {
            return;
        }
        let pdf = "%PDF-1.4
1 0 obj<</Type/Catalog/Pages 2 0 R/Outlines 5 0 R/Names<</Dests<</Names[(later)[4 0 R /Fit]]>>>>>>endobj
2 0 obj<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>endobj
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]>>endobj
4 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]>>endobj
5 0 obj<</Type/Outlines/First 6 0 R/Last 8 0 R/Count 3>>endobj
6 0 obj<</Title(Part & one)/Parent 5 0 R/Dest[3 0 R /Fit]/First 7 0 R/Last 7 0 R/Count 1/Next 8 0 R>>endobj
7 0 obj<</Title<FEFF00430061006600E9>/Parent 6 0 R/A<</S/GoTo/D(later)>>>>endobj
8 0 obj<</Title(Web)/Parent 5 0 R/Prev 6 0 R/A<</S/URI/URI(https://example.com/)>>>>endobj
trailer<</Root 1 0 R>>
";
        let dir = tempfile::tempdir().unwrap();
        let book = open_book(&write(dir.path(), "renamed.cbz", pdf.as_bytes()));
        let want = [Chapter { title: "Part & one".into(), page: 0, depth: 0 }, Chapter { title: "Café".into(), page: 1, depth: 1 }];
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| assert_eq!(book.chapters(&Cancel::new()).unwrap(), want));
            }
        });
    }

    #[test]
    fn djvu_outline_destinations() {
        if !have(&["c44", "djvm", "djvused"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let first = djvu_page(dir.path(), "first", (64, 32), 0);
        let second = djvu_page(dir.path(), "second", (64, 32), 0);
        let path = dir.path().join("book.djvu");
        tool("djvm", &["-c", path.to_str().unwrap(), first.to_str().unwrap(), second.to_str().unwrap()]);
        let script =
            write(dir.path(), "outline.dsed", b"set-outline\n(bookmarks (\"First\" \"#1\" (\"Caf\\303\\251\" \"#second.djvu\")))\n.\nsave\n");
        tool("djvused", &["-f", script.to_str().unwrap(), path.to_str().unwrap()]);
        let book = open_book(&path);
        let want = [Chapter { title: "First".into(), page: 0, depth: 0 }, Chapter { title: "Café".into(), page: 1, depth: 1 }];
        assert_eq!(book.chapters(&Cancel::new()).unwrap(), want);
    }

    #[test]
    fn a_closed_book_stops_its_tools() {
        if !have(&["pdfinfo", "pdftoppm"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let book = open_book(&write(dir.path(), "one.pdf", &pdf(&["/MediaBox[0 0 100 100]"])));
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(book.page(0, &cancel).unwrap_err().is_cancelled());
        assert!(book.chapters(&cancel).unwrap_err().is_cancelled());
        book.close();
        let cancel = Cancel::new();
        assert!(book.page(0, &cancel).unwrap_err().is_cancelled());
        assert!(book.page_size(0, &cancel).unwrap_err().is_cancelled());
    }

    #[test]
    fn encoded_pages_convert_to_png_once() {
        if !have(&["pdfinfo", "pdftoppm"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let book = open_book(&write(dir.path(), "nine.pdf", &pdf(&["/MediaBox[0 0 100 50]"; 9])));
        let cancel = Cancel::new();
        let first: Vec<Encoded> = thread::scope(|scope| {
            let requests: Vec<_> = (0..4).map(|_| scope.spawn(|| book.encoded(0, &Cancel::new()).unwrap())).collect();
            requests.into_iter().map(|request| request.join().unwrap()).collect()
        });
        assert!(first.iter().all(|encoded| encoded.mime == "image/png" && Arc::ptr_eq(&encoded.bytes, &first[0].bytes)), "one conversion");
        assert_eq!(decode(&first[0].bytes, &cancel).unwrap(), *book.page(0, &cancel).unwrap(), "the PNG keeps the pixels");
        for page in 1..9 {
            book.encoded(page, &cancel).unwrap();
        }
        assert!(!Arc::ptr_eq(&book.encoded(0, &cancel).unwrap().bytes, &first[0].bytes), "the oldest page left the cache");
        assert!(matches!(book.encoded(9, &cancel), Err(Error::Format(_))));
        book.close();
        assert!(book.encoded(8, &cancel).unwrap_err().is_cancelled(), "a closed book serves no cached page");
    }

    #[test]
    fn a_displayed_page_reaches_the_browser_without_drawing_again() {
        if !have(&["pdfinfo", "pdftoppm"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "shown.pdf", &pdf(&["/MediaBox[0 0 100 50]"; 2]));
        let book = yoyomi_core::PageCache::new(open_book(&path));
        let cancel = Cancel::new();
        let shown = book.page(0, &cancel).unwrap();
        fs::remove_file(&path).unwrap();
        let encoded = book.encoded(0, &cancel).unwrap();
        assert_eq!(encoded.mime, "image/png");
        assert_eq!(decode(&encoded.bytes, &cancel).unwrap(), *shown);
        assert!(book.encoded(1, &cancel).is_err(), "a page the cache lacks is drawn again");
    }

    #[test]
    fn unusable_search_text_leaves_pdf_search_to_the_text_layers() {
        if !have(&["pdftotext", "pdftohtml"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        // Output that does not split into two pages, and a failing run.
        for script in ["printf 'one\\f\\ftwo\\f'", "exit 1"] {
            let book = RenderedBook {
                path: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pdf/linked.pdf"),
                title: "linked".into(),
                tool: Tool::Pdf,
                pages: 2,
                closed: Cancel::new(),
                sizes: Mutex::new(None),
                chapters: Mutex::new(None),
                text: TextLayers::default(),
                search_pages: Mutex::new(None),
                encoded_pages: Mutex::default(),
            };
            let pdftotext = || {
                let mut command = Command::new("sh");
                command.args(["-c", &format!("echo run >> '{}'; {script}", runs.display())]);
                command
            };
            let cancel = Cancel::new();
            for page in [0, 1, 0] {
                assert_eq!(book.search_text_with(page, &cancel, pdftotext()).unwrap(), None, "{script}");
            }
            assert_eq!(book.text_layer(1, &cancel).unwrap().unwrap().lines[0].text, "Second", "{script}");
        }
        assert_eq!(fs::read_to_string(&runs).unwrap().lines().count(), 2, "each book ran the document once");
    }

    #[test]
    fn a_cancelled_request_stops_waiting_for_a_lock() {
        let mutex = Mutex::new(());
        let held = mutex.lock();
        let cancel = Cancel::new();
        thread::scope(|scope| {
            let waiter = scope.spawn(|| lock(&mutex, &[&cancel]).map(|_| ()));
            thread::sleep(POLL * 4);
            cancel.cancel();
            assert!(waiter.join().unwrap().unwrap_err().is_cancelled());
        });
        drop(held);
        assert!(lock(&mutex, &[&Cancel::new()]).is_ok());
    }
}
