//! Text layers: the words, lines and links of PDF pages from pdftotext and
//! pdftohtml, and the plain text of DJVU pages from djvused.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::thread;

use parking_lot::Mutex;
use yoyomi_core::{Cancel, Error, Link, LinkSpan, Result, TextBox, TextLayer, TextLine};

use super::tool::run;
use super::{Tool, lock};

/// How many page layers a book keeps.
const MAX_LAYERS: usize = 64;

/// The most bytes pdftotext and pdftohtml may print for one page.
const MAX_PAGE_XML_BYTES: usize = 16 << 20;

/// The most bytes pdfinfo may print about the boxes of one page.
const MAX_BOX_BYTES: usize = 64 << 10;

/// The most bytes djvused may print for the text of one page.
const MAX_DJVU_TEXT_BYTES: usize = 16 << 20;

/// The text layers of one document, read on demand.
#[derive(Default)]
pub(super) struct TextLayers {
    pages: Mutex<Slots>,
}

/// The page layers, the most recent `MAX_LAYERS` of them. Each slot has
/// its own lock, so concurrent requests for one page share one extraction
/// and different pages extract side by side.
#[derive(Default)]
struct Slots {
    by_page: HashMap<usize, Slot>,
    /// Filled pages, oldest first.
    order: VecDeque<usize>,
}

type Slot = Arc<Mutex<Option<Arc<TextLayer>>>>;

impl TextLayers {
    /// The layer of `page`, one of `pages`. A PDF layer has word boxes and
    /// links; a DJVU layer has its lines of text alone. Failed extractions
    /// give an empty layer and stay cached; cancelled ones cache nothing.
    pub(super) fn layer(&self, path: &Path, tool: Tool, pages: usize, page: usize, stops: &[&Cancel]) -> Result<Arc<TextLayer>> {
        match tool {
            Tool::Pdf => self.cached(page, stops, || pdf_layer(path, page, pages, stops)),
            Tool::Djvu => self.cached(page, stops, || djvu_layer(path, page, stops)),
        }
    }

    /// The cached layer of `page`, or the one `extract` reads.
    fn cached(&self, page: usize, stops: &[&Cancel], extract: impl FnOnce() -> Result<TextLayer>) -> Result<Arc<TextLayer>> {
        let slot = self.pages.lock().by_page.entry(page).or_default().clone();
        let (layer, read) = match fill(&slot, stops, extract) {
            Ok(filled) => filled,
            Err(error) => {
                self.abandon(page, slot);
                return Err(error);
            }
        };
        if read {
            let mut slots = self.pages.lock();
            slots.order.push_back(page);
            while slots.order.len() > MAX_LAYERS {
                if let Some(oldest) = slots.order.pop_front() {
                    slots.by_page.remove(&oldest);
                }
            }
        }
        Ok(layer)
    }

    /// Drops the slot of `page` that a cancelled request held, unless it
    /// has a layer or another request holds it too.
    fn abandon(&self, page: usize, slot: Slot) {
        let mut slots = self.pages.lock();
        // Requests clone slots and abandon them under this lock, so the last
        // request to give up an empty slot sees two holders: the map and itself.
        if slots.by_page.get(&page).is_some_and(|kept| Arc::ptr_eq(kept, &slot)) && Arc::strong_count(&slot) == 2 && slot.lock().is_none() {
            slots.by_page.remove(&page);
        }
        drop(slot);
    }
}

/// The layer in `slot`, or the one `extract` reads into it, and whether this
/// call read it. Only cancellation fails.
fn fill(slot: &Slot, stops: &[&Cancel], extract: impl FnOnce() -> Result<TextLayer>) -> Result<(Arc<TextLayer>, bool)> {
    let mut layer = lock(slot, stops)?;
    if let Some(layer) = &*layer {
        return Ok((layer.clone(), false));
    }
    let read = match extract() {
        Err(error) if error.is_cancelled() => return Err(error),
        // A failed page stays empty instead of running the tools again on every request.
        read => Arc::new(read.unwrap_or_default()),
    };
    *layer = Some(read.clone());
    Ok((read, true))
}

/// Reads the words of PDF page `page` with pdftotext, in the crop box space
/// pdftoppm draws, and the link rectangles with pdftohtml.
fn pdf_layer(path: &Path, page: usize, pages: usize, stops: &[&Cancel]) -> Result<TextLayer> {
    let n = (page + 1).to_string();
    let no_words = Cancel::new();
    thread::scope(|scope| {
        // pdftohtml parses the document again, so it runs alongside pdftotext
        // and stops when the words do not come.
        let links = scope.spawn(|| {
            let mut stops = stops.to_vec();
            stops.push(&no_words);
            let mut command = Command::new("pdftohtml");
            command.args(["-xml", "-i", "-q", "-enc", "UTF-8", "-zoom", "1", "-f", &n, "-l", &n, "-stdout"]).arg(path);
            run(&mut command, MAX_PAGE_XML_BYTES, &stops)
        });
        let mut command = Command::new("pdftotext");
        command.args(["-cropbox", "-bbox-layout", "-f", &n, "-l", &n]).arg(path).arg("-");
        let words = run(&mut command, MAX_PAGE_XML_BYTES, stops);
        if words.is_err() {
            no_words.cancel();
        }
        let links = links.join().unwrap_or_else(|_| Err(Error::Format("pdftohtml reader panicked".into())));
        let words = words?;
        stops.iter().try_for_each(|stop| stop.check())?;
        let links = links.ok();
        let crop_offset = || pdf_crop_offset(path, &n, stops);
        pdf_text_layer(&String::from_utf8_lossy(&words), links.as_deref().map(String::from_utf8_lossy).as_deref(), crop_offset, pages)
    })
}

/// Builds a page layer from pdftotext's `-bbox-layout` XML and pdftohtml's
/// XML of the same page. A block of pdftotext is a paragraph; its lines end
/// in a space until the last. `crop_offset` says where the crop box sits in
/// the media box, for pages whose boxes differ.
fn pdf_text_layer(words: &str, links: Option<&str>, crop_offset: impl FnOnce() -> (f64, f64), pages: usize) -> Result<TextLayer> {
    let doc = parse_xml(words, "pdftotext")?;
    let Some(page) = doc.descendants().find(|node| node.has_tag_name("page")) else { return Ok(TextLayer::default()) };
    let (w, h) = (number(page, "width"), number(page, "height"));
    if !(w > 0.0 && h > 0.0) {
        return Ok(TextLayer::default());
    }
    let links = links.and_then(|xml| pdf_links(xml, w, h, crop_offset).ok()).unwrap_or_default();
    let mut lines: Vec<TextLine> = Vec::new();
    for block in elements(page).flat_map(elements) {
        let first = lines.len();
        let rows: Vec<_> = elements(block).filter(|row| row.has_tag_name("line")).collect();
        for (r, row) in rows.iter().enumerate() {
            let end = r + 1 == rows.len();
            let mut line = TextLine { text: String::new(), top: f64::INFINITY, bottom: f64::NEG_INFINITY, end, boxes: Vec::new(), links: Vec::new() };
            let mut count = 0;
            for word in elements(*row).filter(|word| word.has_tag_name("word")) {
                let text = visible(&word.descendants().filter(roxmltree::Node::is_text).filter_map(|node| node.text()).collect::<String>());
                if text.is_empty() {
                    continue;
                }
                if count > 0 {
                    line.text.push(' ');
                    count += 1;
                }
                let runes = text.chars().count();
                line.boxes.push(TextBox { x0: number(word, "xMin") / w, x1: number(word, "xMax") / w, index: count, count: runes, rtl: false });
                line.text.push_str(&text);
                count += runes;
                line.top = line.top.min(number(word, "yMin") / h);
                line.bottom = line.bottom.max(number(word, "yMax") / h);
            }
            if line.boxes.is_empty() {
                continue;
            }
            if !line.end {
                line.text.push(' ');
            }
            line.links = line_links(&line, &links, pages);
            lines.push(line);
        }
        // A last row without visible words must not join the paragraph to the next.
        if let Some(last) = lines[first..].last_mut()
            && !last.end
        {
            last.end = true;
            last.text.pop();
        }
    }
    Ok(TextLayer { lines })
}

/// A link rectangle in crop box fractions.
struct PdfLink {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    href: String,
}

/// Reads the link rectangles of a page, as crop box fractions, from
/// pdftohtml's XML. pdftohtml works in the media box, so `crop_offset`
/// supplies the shift when the boxes differ in size.
fn pdf_links(xml: &str, crop_w: f64, crop_h: f64, crop_offset: impl FnOnce() -> (f64, f64)) -> Result<Vec<PdfLink>> {
    let doc = parse_xml(xml, "pdftohtml")?;
    let Some(page) = doc.root_element().children().find(|node| node.has_tag_name("page")) else { return Ok(Vec::new()) };
    let (media_w, media_h) = (number(page, "width"), number(page, "height"));
    let (dx, dy) = if (media_w - crop_w).abs() > 1.0 || (media_h - crop_h).abs() > 1.0 { crop_offset() } else { (0.0, 0.0) };
    let mut links = Vec::new();
    let runes = |node: roxmltree::Node| {
        node.descendants().filter(roxmltree::Node::is_text).filter_map(|node| node.text()).map(|text| text.chars().count()).sum::<usize>()
    };
    for text in page.children().filter(|node| node.has_tag_name("text")) {
        let (left, top) = (number(text, "left") - dx, number(text, "top") - dy);
        let (width, bottom) = (number(text, "width"), top + number(text, "height"));
        // A lone anchor takes the whole element. Two or more each take the
        // share of the element's width that their runes take of its text,
        // so they keep apart.
        // shortcut: runes count as equally wide; measure glyphs if narrow words next to wide ones pick the wrong link.
        let mut offset = 0;
        let mut anchors = Vec::new();
        for node in text.descendants() {
            if node.is_text() {
                offset += node.text().map_or(0, |text| text.chars().count());
            } else if node.has_tag_name("a")
                && let Some(href) = node.attribute("href").filter(|href| !href.is_empty())
            {
                anchors.push((offset, offset + runes(node), href));
            }
        }
        let total = offset.max(1) as f64;
        if let [(_, _, href)] = anchors[..] {
            links.push(PdfLink { x0: left / crop_w, y0: top / crop_h, x1: (left + width) / crop_w, y1: bottom / crop_h, href: href.to_owned() });
            continue;
        }
        for (start, end, href) in anchors {
            let (x0, x1) = (left + width * start as f64 / total, left + width * end as f64 / total);
            links.push(PdfLink { x0: x0 / crop_w, y0: top / crop_h, x1: x1 / crop_w, y1: bottom / crop_h, href: href.to_owned() });
        }
    }
    Ok(links)
}

/// Asks pdfinfo how far the crop box origin of page `n` sits from the media
/// box origin in display space: left and top, in points.
fn pdf_crop_offset(path: &Path, n: &str, stops: &[&Cancel]) -> (f64, f64) {
    let mut command = Command::new("pdfinfo");
    command.args(["-f", n, "-l", n, "-box"]).arg(path);
    run(&mut command, MAX_BOX_BYTES, stops).map_or((0.0, 0.0), |out| crop_offset(&String::from_utf8_lossy(&out)))
}

/// Reads `Page <n> MediaBox: x0 y0 x1 y1` and the `CropBox` line from
/// `pdfinfo -box` and returns the left and top offset of the crop box.
fn crop_offset(info: &str) -> (f64, f64) {
    let mut boxes: HashMap<&str, [f64; 4]> = HashMap::new();
    for line in info.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if let ["Page", _, name, numbers @ ..] = fields.as_slice()
            && let [Ok(x0), Ok(y0), Ok(x1), Ok(y1)] = numbers.iter().map(|n| n.parse::<f64>()).collect::<Vec<_>>().as_slice()
        {
            boxes.insert(*name, [*x0, *y0, *x1, *y1]);
        }
    }
    match (boxes.get("MediaBox:"), boxes.get("CropBox:")) {
        (Some(media), Some(crop)) => (crop[0] - media[0], media[3] - crop[3]),
        _ => (0.0, 0.0),
    }
}

/// Joins adjacent words whose centers fall in the same link rectangle into
/// one link span.
fn line_links(line: &TextLine, links: &[PdfLink], pages: usize) -> Vec<LinkSpan> {
    let mut spans: Vec<LinkSpan> = Vec::new();
    let y = (line.top + line.bottom) / 2.0;
    for word in &line.boxes {
        let x = (word.x0 + word.x1) / 2.0;
        let href = links.iter().find(|link| (link.x0..=link.x1).contains(&x) && (link.y0..=link.y1).contains(&y));
        let Some(target) = href.and_then(|link| link_target(&link.href, pages)) else { continue };
        match spans.last_mut() {
            Some(last) if last.target == target && word.index == last.end + 1 => last.end = word.index + word.count,
            _ => spans.push(LinkSpan { start: word.index, end: word.index + word.count, target }),
        }
    }
    spans
}

/// Keeps web and mail links and turns the page references pdftohtml writes
/// as `name.html#N` into page `N` of `pages`. Other schemes drop out.
fn link_target(href: &str, pages: usize) -> Option<Link> {
    let scheme = href.split_once(':').map(|(scheme, _)| scheme).filter(|scheme| {
        let mut chars = scheme.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    if let Some(scheme) = scheme {
        return ["http", "https", "mailto"].contains(&scheme.to_ascii_lowercase().as_str()).then(|| Link::Url(href.to_owned()));
    }
    // The document name precedes the page and may itself hold a '#'.
    let (_, n) = href.rsplit_once('#')?;
    n.parse::<usize>().ok().filter(|n| (1..=pages).contains(n)).map(|n| Link::Page(n - 1))
}

/// Reads the text of DJVU page `page` with djvused, which selects it by
/// number and ends its text with a form feed.
fn djvu_layer(path: &Path, page: usize, stops: &[&Cancel]) -> Result<TextLayer> {
    let script = format!("select {}; print-pure-txt", page + 1);
    let text = run(Command::new("djvused").args(["-u", "-e", &script]).arg(path), MAX_DJVU_TEXT_BYTES, stops)?;
    Ok(plain_layer(&String::from_utf8_lossy(&text)))
}

/// A layer of plain lines without boxes, one per line of `text`; form feeds
/// count as blank space.
fn plain_layer(text: &str) -> TextLayer {
    let lines = text
        .lines()
        .map(|line| visible(line).trim().to_owned())
        .filter(|line| !line.is_empty())
        .map(|text| TextLine { text, top: 0.0, bottom: 0.0, end: true, boxes: Vec::new(), links: Vec::new() })
        .collect();
    TextLayer { lines }
}

/// Splits the pdftotext output of a document of `pages` pages into the text
/// of each page, without the invisible characters selection drops too.
/// pdftotext ends every page, an empty one too, with a form feed. A stray
/// form feed would move every later page, so a count that does not match
/// gives no pages and leaves search to the text layers.
pub(super) fn search_pages(text: &str, pages: usize) -> Vec<String> {
    let split: Vec<&str> = text.split('\u{c}').collect();
    if split.len() != pages + 1 {
        return Vec::new();
    }
    split.into_iter().map(visible).collect()
}

/// `text` without the invisible characters PDFs hide in words.
fn visible(text: &str) -> String {
    text.chars().filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{ad}')).collect()
}

fn parse_xml<'a>(text: &'a str, tool: &str) -> Result<roxmltree::Document<'a>> {
    let options = roxmltree::ParsingOptions { allow_dtd: true, ..roxmltree::ParsingOptions::default() };
    roxmltree::Document::parse_with_options(text, options).map_err(|error| Error::Format(format!("{tool} output: {error}")))
}

fn elements<'a, 'input>(node: roxmltree::Node<'a, 'input>) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    node.children().filter(roxmltree::Node::is_element)
}

/// The number in attribute `name`, or 0.
fn number(node: roxmltree::Node, name: &str) -> f64 {
    node.attribute(name).and_then(|value| value.trim().parse().ok()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use yoyomi_core::Book;

    use super::*;
    use crate::Opened;

    fn have(tools: &[&str]) -> bool {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let missing = tools.iter().find(|tool| !std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()));
        if let Some(tool) = missing {
            eprintln!("skipped: {tool} is not installed");
        }
        missing.is_none()
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

    /// Two pages of Helvetica text with a web link over "visit example" and a
    /// page link over "next page". The cropped copy trims the first page's
    /// crop box so the media and crop boxes differ.
    fn linked_pdf(crop: bool) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pdf").join(if crop { "cropped.pdf" } else { "linked.pdf" })
    }

    #[test]
    fn pdf_text_layer_selection_and_links() {
        if !have(&["pdfinfo", "pdftotext", "pdftohtml"]) {
            return;
        }
        for crop in [false, true] {
            let book = open_book(&linked_pdf(crop));
            let cancel = Cancel::new();
            let layer = book.text_layer(0, &cancel).unwrap().unwrap();
            let lines = &layer.lines;
            let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
            assert_eq!(texts, ["Hello reader", "visit example", "next page"], "crop box {crop}");
            assert!(lines.iter().all(|line| line.end && line.boxes.len() == 2));
            assert!(lines[0].boxes[0].x0 > 0.0 && lines[0].top < lines[0].bottom && lines[2].bottom <= 1.0);
            assert_eq!(lines[0].links, []);
            assert_eq!(lines[1].links, [LinkSpan { start: 0, end: 13, target: Link::Url("https://example.com/a?utm_source=pdf".into()) }]);
            assert_eq!(lines[2].links, [LinkSpan { start: 0, end: 9, target: Link::Page(1) }]);
            assert_eq!(book.text_layer(1, &cancel).unwrap().unwrap().lines[0].text, "Second");
            assert!(matches!(book.text_layer(2, &cancel), Err(Error::Format(_))));
        }
    }

    #[test]
    fn link_targets_keep_web_mail_and_page_links() {
        assert_eq!(link_target("Tricky name.html#12", 20), Some(Link::Page(11)));
        assert_eq!(link_target("issue#2.html#12", 20), Some(Link::Page(11)), "a '#' in the document name");
        assert_eq!(link_target("book.html#21", 20), None, "a page past the end");
        assert_eq!(link_target("file:///etc/passwd", 20), None);
        assert_eq!(link_target("javascript:alert(1)#3", 20), None);
        assert_eq!(link_target("MAILTO:me@example.com", 20), Some(Link::Url("MAILTO:me@example.com".into())));
        assert_eq!(link_target("", 20), None);
        assert_eq!(visible("skylining@live.com\u{200b}"), "skylining@live.com");
    }

    #[test]
    fn a_paragraph_ends_at_its_last_visible_line() {
        // The first block ends with a row that holds only a zero-width space.
        let words = "<html><body><doc><page width=\"200\" height=\"100\"><flow>\
            <block><line><word xMin=\"10\" yMin=\"10\" xMax=\"80\" yMax=\"20\">First</word></line>\
            <line><word xMin=\"10\" yMin=\"30\" xMax=\"20\" yMax=\"40\">\u{200b}</word></line></block>\
            <block><line><word xMin=\"10\" yMin=\"50\" xMax=\"80\" yMax=\"60\">Second</word></line></block>\
            </flow></page></doc></body></html>";
        let links = "<pdf2xml><page width=\"200\" height=\"100\"/></pdf2xml>";
        let layer = pdf_text_layer(words, Some(links), || unreachable!("the boxes match"), 1).unwrap();
        let lines: Vec<(&str, bool)> = layer.lines.iter().map(|line| (line.text.as_str(), line.end)).collect();
        assert_eq!(lines, [("First", true), ("Second", true)]);
        assert_eq!(layer.lines[0].boxes[0], TextBox { x0: 0.05, x1: 0.4, index: 0, count: 5, rtl: false });
        assert_eq!((layer.lines[0].top, layer.lines[0].bottom), (0.1, 0.2));
    }

    #[test]
    fn two_links_in_one_text_element_keep_their_own_targets() {
        let words = "<html><body><doc><page width=\"200\" height=\"100\"><flow><block><line>\
            <word xMin=\"10\" yMin=\"10\" xMax=\"40\" yMax=\"20\">one</word>\
            <word xMin=\"60\" yMin=\"10\" xMax=\"90\" yMax=\"20\">two</word>\
            </line></block></flow></page></doc></body></html>";
        let links = "<pdf2xml><page width=\"200\" height=\"100\"><text top=\"10\" left=\"10\" width=\"80\" height=\"10\">\
            <a href=\"https://a.example/\">one</a> <a href=\"book.html#2\"><b>two</b></a></text></page></pdf2xml>";
        let layer = pdf_text_layer(words, Some(links), || unreachable!("the boxes match"), 2).unwrap();
        let want =
            [LinkSpan { start: 0, end: 3, target: Link::Url("https://a.example/".into()) }, LinkSpan { start: 4, end: 7, target: Link::Page(1) }];
        assert_eq!(layer.lines[0].links, want);
    }

    #[test]
    fn a_lone_link_takes_its_whole_text_element() {
        // By character count the link would start at x = 65, past the center
        // of "link". The whole element covers "see" too.
        let words = "<html><body><doc><page width=\"200\" height=\"100\"><flow><block><line>\
            <word xMin=\"10\" yMin=\"10\" xMax=\"20\" yMax=\"20\">see</word>\
            <word xMin=\"30\" yMin=\"10\" xMax=\"80\" yMax=\"20\">link</word>\
            </line></block></flow></page></doc></body></html>";
        let links = "<pdf2xml><page width=\"200\" height=\"100\"><text top=\"10\" left=\"10\" width=\"70\" height=\"10\">\
            see wide words <a href=\"https://a.example/\">link</a></text></page></pdf2xml>";
        let layer = pdf_text_layer(words, Some(links), || unreachable!("the boxes match"), 1).unwrap();
        assert_eq!(layer.lines[0].links, [LinkSpan { start: 0, end: 8, target: Link::Url("https://a.example/".into()) }]);
    }

    #[test]
    fn crop_offsets_come_from_pdfinfo_boxes() {
        let info = "Page    1 MediaBox:     0.00     0.00   200.00   100.00\nPage    1 CropBox:      5.00    10.00   200.00    95.00\n";
        assert_eq!(crop_offset(info), (5.0, 5.0));
        assert_eq!(crop_offset("Page 1 MediaBox: 0 0 200 100\n"), (0.0, 0.0));
    }

    #[test]
    fn search_text_reads_pdf_and_djvu_layers() {
        let words = |layer: Option<Arc<TextLayer>>| layer.map(|layer| layer.lines.iter().map(|line| line.text.clone()).collect::<Vec<_>>().join(" "));
        let dir = tempfile::tempdir().unwrap();
        if have(&["pdfinfo", "pdftotext", "pdftohtml"]) {
            let book = open_book(&linked_pdf(false));
            let cancel = Cancel::new();
            let first = words(book.text_layer(0, &cancel).unwrap()).unwrap();
            assert_eq!(first.split_whitespace().collect::<Vec<_>>(), ["Hello", "reader", "visit", "example", "next", "page"]);
            assert_eq!(words(book.text_layer(1, &cancel).unwrap()).as_deref(), Some("Second"));
        }
        if !have(&["c44", "djvm", "djvused"]) {
            return;
        }
        // The middle page has no text layer.
        let path = djvu_scan(dir.path(), &[Some("Hello"), None, Some("Привет")], None);
        let book = open_book(&path);
        let cancel = Cancel::new();
        let got: Vec<Option<String>> = (0..3).map(|page| words(book.text_layer(page, &cancel).unwrap())).collect();
        assert_eq!(got, [Some("Hello".to_owned()), None, Some("Привет".to_owned())]);
        assert!(book.text_layer(0, &cancel).unwrap().unwrap().lines[0].boxes.is_empty(), "DJVU text has no boxes");
    }

    /// Two pages of the same Helvetica words; the first page's crop box
    /// leaves them out.
    fn cropped_words_pdf(dir: &Path) -> PathBuf {
        let content = "BT /F1 12 Tf 10 10 Td (Outside words) Tj ET";
        let page = |n: usize, crop: &str| {
            format!("{n} 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]{crop}/Resources<</Font<</F1 4 0 R>>>>/Contents 5 0 R>>endobj\n")
        };
        let pdf = format!(
            "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R 6 0 R]/Count 2>>endobj\n{}\
             4 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n5 0 obj<</Length {}>>stream\n{content}\nendstream\nendobj\n{}\
             trailer<</Root 1 0 R>>\n",
            page(3, "/CropBox[100 50 200 100]"),
            content.len(),
            page(6, ""),
        );
        let path = dir.join("cropped-words.pdf");
        fs::write(&path, pdf).unwrap();
        path
    }

    #[test]
    fn pdf_search_text_reads_the_whole_document_once() {
        if !have(&["pdfinfo", "pdftotext", "pdftohtml"]) {
            return;
        }
        let cancel = Cancel::new();
        let book = open_book(&linked_pdf(false));
        let first = book.search_text(0, &cancel).unwrap().unwrap();
        assert_eq!(first.split_whitespace().collect::<Vec<_>>(), ["Hello", "reader", "visit", "example", "next", "page"]);
        assert_eq!(book.search_text(1, &cancel).unwrap().unwrap().trim(), "Second");
        assert!(matches!(book.search_text(2, &cancel), Err(Error::Format(_))));
        let dir = tempfile::tempdir().unwrap();
        let path = cropped_words_pdf(dir.path());
        let book = open_book(&path);
        assert_eq!(book.text_layer(0, &cancel).unwrap(), None, "selection keeps to the crop box");
        assert_eq!(book.search_text(0, &cancel).unwrap().unwrap().trim(), "Outside words", "search reads the media box");
        fs::remove_file(&path).unwrap();
        assert_eq!(book.search_text(1, &cancel).unwrap().unwrap().trim(), "Outside words", "the text stays while the book is open");
    }

    #[test]
    fn pdf_search_text_splits_at_form_feeds() {
        assert_eq!(search_pages("co\u{200b}oper\u{ad}ate\u{c}second\u{c}", 2), ["cooperate", "second", ""]);
        assert!(search_pages("one\u{c}\u{c}two\u{c}", 2).is_empty(), "a stray form feed misaligns the pages");
    }

    /// A bundled DJVU of blank pages, each with a one-word text layer or
    /// none, and a shared annotation, which must not shift the pages. Page
    /// `broken` gets a corrupt text chunk.
    fn djvu_scan(dir: &Path, words: &[Option<&str>], broken: Option<usize>) -> PathBuf {
        let source = dir.join("page.ppm");
        let mut ppm = b"P6\n64 32\n255\n".to_vec();
        ppm.extend([255].repeat(64 * 32 * 3));
        fs::write(&source, ppm).unwrap();
        let mut pages = Vec::new();
        for (i, word) in words.iter().enumerate() {
            let page = dir.join(format!("{i}.djvu"));
            tool("c44", &[source.to_str().unwrap(), page.to_str().unwrap()]);
            if let Some(word) = word {
                let layer = dir.join(format!("{i}.txt"));
                fs::write(&layer, format!("(page 0 0 64 32 (word 0 0 64 32 \"{word}\"))")).unwrap();
                tool("djvused", &["-e", &format!("select 1; set-txt {}", layer.display()), "-s", page.to_str().unwrap()]);
            }
            if broken == Some(i) {
                let mut bytes = fs::read(&page).unwrap();
                let at = bytes.windows(4).position(|id| id == b"TXTz").expect("a text chunk") + 8;
                let len = u32::from_be_bytes(bytes[at - 4..at].try_into().unwrap()) as usize;
                bytes[at..at + len].fill(0xff);
                fs::write(&page, bytes).unwrap();
            }
            pages.push(page.to_str().unwrap().to_owned());
        }
        let path = dir.join("scan.djvu");
        let mut args = vec!["-c", path.to_str().unwrap()];
        args.extend(pages.iter().map(String::as_str));
        tool("djvm", &args);
        tool("djvused", &["-e", "create-shared-ant; save", path.to_str().unwrap()]);
        path
    }

    #[test]
    fn djvu_pages_read_alone_into_the_bounded_cache() {
        if !have(&["c44", "djvm", "djvused"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = djvu_scan(dir.path(), &[Some("Hello"), Some("Broken"), Some("Bye")], Some(1));
        let layers = TextLayers::default();
        let stops: &[&Cancel] = &[&Cancel::new()];
        let text = |page| layers.layer(&path, Tool::Djvu, 3, page, stops).unwrap().lines.iter().map(|line| line.text.clone()).collect::<Vec<_>>();
        assert_eq!(text(2), ["Bye"]);
        assert_eq!(layers.pages.lock().by_page.len(), 1, "one request reads one page");
        assert_eq!(text(1), Vec::<String>::new(), "the corrupt page reads empty");
        assert_eq!(text(0), ["Hello"], "the pages beside it keep their text");
        // Pages past the end fail in djvused and fill the cache with empty layers.
        for page in 3..MAX_LAYERS + 3 {
            assert!(text(page).is_empty());
        }
        assert_eq!(layers.pages.lock().by_page.len(), MAX_LAYERS);
        assert!(!layers.pages.lock().by_page.contains_key(&2), "the oldest page was evicted");
    }

    #[test]
    fn plain_layers_drop_invisible_characters_and_blank_lines() {
        let layer = plain_layer(" co\u{200b}oper\u{ad}ate \n\n second line\n\u{c}");
        let lines: Vec<&str> = layer.lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(lines, ["cooperate", "second line"]);
    }

    #[test]
    fn the_layer_cache_is_bounded() {
        let layers = TextLayers::default();
        let calls = AtomicUsize::new(0);
        let read = |page: usize| {
            let calls = &calls;
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(plain_layer(&page.to_string()))
            }
        };
        let stops: &[&Cancel] = &[&Cancel::new()];
        for page in 0..=MAX_LAYERS {
            assert_eq!(layers.cached(page, stops, read(page)).unwrap().lines[0].text, page.to_string());
        }
        assert_eq!(calls.load(Ordering::SeqCst), MAX_LAYERS + 1);
        layers.cached(MAX_LAYERS, stops, read(MAX_LAYERS)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), MAX_LAYERS + 1, "a cached page reads nothing");
        layers.cached(0, stops, read(0)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), MAX_LAYERS + 2, "the oldest page was evicted");
        assert_eq!(layers.pages.lock().by_page.len(), MAX_LAYERS);
    }

    #[test]
    fn concurrent_requests_share_one_extraction() {
        let layers = TextLayers::default();
        let calls = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    let layer = layers.cached(0, &[&Cancel::new()], || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(100));
                        Ok(plain_layer("Hello"))
                    });
                    assert_eq!(layer.unwrap().lines[0].text, "Hello");
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failures_stay_cached_and_cancellations_do_not() {
        let layers = TextLayers::default();
        let stops: &[&Cancel] = &[&Cancel::new()];
        assert!(layers.cached(0, stops, || Err(Error::Cancelled)).unwrap_err().is_cancelled());
        assert_eq!(layers.cached(0, stops, || Ok(plain_layer("later"))).unwrap().lines[0].text, "later");
        let failed = layers.cached(1, stops, || Err(Error::Format("broken".into()))).unwrap();
        assert!(failed.lines.is_empty());
        assert!(layers.cached(1, stops, || panic!("a failed page ran its tools again")).unwrap().lines.is_empty());
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(layers.cached(2, &[&cancel], || panic!("a cancelled request extracted")).unwrap_err().is_cancelled());
    }

    #[test]
    fn cancelled_requests_leave_no_slots() {
        let layers = TextLayers::default();
        let cancel = Cancel::new();
        cancel.cancel();
        for page in 0..2 * MAX_LAYERS {
            assert!(layers.cached(page, &[&Cancel::new()], || Err(Error::Cancelled)).unwrap_err().is_cancelled());
            assert!(layers.cached(page, &[&cancel], || panic!("a cancelled request extracted")).unwrap_err().is_cancelled());
        }
        assert!(layers.pages.lock().by_page.is_empty());
        layers.cached(0, &[&Cancel::new()], || Ok(plain_layer("kept"))).unwrap();
        assert!(layers.cached(0, &[&cancel], || panic!("a cancelled request extracted")).unwrap_err().is_cancelled());
        assert_eq!(layers.pages.lock().by_page.len(), 1, "a filled slot stays");
    }

    #[test]
    fn a_closed_book_stops_text_extraction() {
        if !have(&["pdfinfo"]) {
            return;
        }
        let book = open_book(&linked_pdf(false));
        book.close();
        assert!(book.text_layer(0, &Cancel::new()).unwrap_err().is_cancelled());
    }
}
