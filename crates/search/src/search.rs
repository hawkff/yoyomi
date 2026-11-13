//! A search through one book, page by page on the job pool.

use std::collections::HashMap;
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use yoyomi_core::{Book, Cancel, Error, Jobs, PageKind, Priority, Result};

use crate::ocr::{kill_running, recognize_in, tesseract_available};

/// Text pages one step reads before it hands the worker back.
const BATCH: usize = 64;

/// Ids are unique in the process, so progress from the search of a closed
/// book never counts for the next one.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// `text` folded to lower case with its words joined by single spaces, so a
/// phrase also matches across the line breaks of a text layer or OCR output.
pub fn searchable(text: &str) -> String {
    text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `book` can have text without OCR: text pages, or rendered pages,
/// which may carry a PDF or DJVU text layer.
pub fn may_have_text(book: &dyn Book) -> bool {
    (0..book.len()).any(|page| matches!(book.kind(page), PageKind::Text | PageKind::Rendered))
}

/// What one step of a search found. `Search::on_progress` takes it.
#[derive(Debug)]
pub struct Progress {
    id: u64,
    /// Pages read so far, including this step's.
    scanned: usize,
    hits: Vec<usize>,
    skipped: usize,
}

/// The search of one book and the OCR text it has read, which later searches
/// of the book reuse. Each step reads one page, or up to 64 text pages, on
/// the job pool at idle priority and reports a `Progress`. Dropping the
/// search cancels it.
pub struct Search {
    shared: Arc<Shared>,
    cancel: Cancel,
    /// The tesseract of the running search, which a cancel kills.
    tesseract: Arc<Mutex<Option<Child>>>,
    id: u64,
    term: String,
    hits: Vec<usize>,
    scanned: usize,
    skipped: usize,
    running: bool,
}

struct Shared {
    book: Arc<dyn Book>,
    jobs: Jobs,
    report: Box<dyn Fn(Progress) + Send + Sync>,
    /// OCR text by page.
    ocr: Mutex<HashMap<usize, String>>,
}

/// One started search, as its steps see it.
struct Run {
    shared: Arc<Shared>,
    cancel: Cancel,
    tesseract: Arc<Mutex<Option<Child>>>,
    id: u64,
    term: String,
}

impl Search {
    /// A search of `book` whose steps run on `jobs` and go to `report`.
    pub fn new(book: Arc<dyn Book>, jobs: Jobs, report: impl Fn(Progress) + Send + Sync + 'static) -> Self {
        let shared = Arc::new(Shared { book, jobs, report: Box::new(report), ocr: Mutex::new(HashMap::new()) });
        Search {
            shared,
            cancel: Cancel::new(),
            tesseract: Arc::default(),
            id: 0,
            term: String::new(),
            hits: Vec::new(),
            scanned: 0,
            skipped: 0,
            running: false,
        }
    }

    /// Cancels the search in progress and searches for `term` from the
    /// first page.
    pub fn start(&mut self, term: &str) {
        self.cancel();
        self.id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        self.term = searchable(term);
        self.running = self.total() > 0;
        if self.running {
            let run = Run {
                shared: self.shared.clone(),
                cancel: self.cancel.clone(),
                tesseract: self.tesseract.clone(),
                id: self.id,
                term: self.term.clone(),
            };
            spawn_step(Arc::new(run), 0);
        }
    }

    /// Stops the search, with its tesseract killed and reaped before this
    /// returns, and forgets its term and hits.
    pub fn cancel(&mut self) {
        self.cancel.cancel();
        kill_running(&self.tesseract);
        self.cancel = Cancel::new();
        self.tesseract = Arc::default();
        self.id = 0;
        self.term.clear();
        self.hits.clear();
        (self.scanned, self.skipped, self.running) = (0, 0, false);
    }

    /// Counts a step of the running search and returns whether it ended the
    /// search. Progress from another or a cancelled search changes nothing.
    pub fn on_progress(&mut self, progress: Progress) -> bool {
        if progress.id != self.id || !self.running {
            return false;
        }
        self.hits.extend(progress.hits);
        self.skipped += progress.skipped;
        self.scanned = progress.scanned;
        self.running = self.scanned < self.total();
        !self.running
    }

    /// The first hit after `page`, or the last one before it, wrapping
    /// around the book.
    pub fn hit_from(&self, page: usize, forward: bool) -> Option<usize> {
        if forward {
            self.hits.iter().copied().find(|&hit| hit > page).or_else(|| self.hits.first().copied())
        } else {
            self.hits.iter().copied().rev().find(|&hit| hit < page).or_else(|| self.hits.last().copied())
        }
    }

    /// The term in its searchable form, empty before a search.
    pub fn term(&self) -> &str {
        &self.term
    }

    /// The pages that hold the term, in page order.
    pub fn hits(&self) -> &[usize] {
        &self.hits
    }

    pub fn scanned(&self) -> usize {
        self.scanned
    }

    /// Pages whose text could not be read.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// The number of pages of the book.
    pub fn total(&self) -> usize {
        self.shared.book.len()
    }

    pub fn running(&self) -> bool {
        self.running
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.cancel();
        kill_running(&self.tesseract);
    }
}

fn spawn_step(run: Arc<Run>, page: usize) {
    let jobs = run.shared.jobs.clone();
    let cancel = run.cancel.clone();
    jobs.spawn(Priority::Idle, &cancel, move || step(run, page));
}

/// Reads `page` and the text pages after it, up to `BATCH` pages, reports
/// them and queues the next step.
fn step(run: Arc<Run>, first: usize) {
    let book = &run.shared.book;
    let mut progress = Progress { id: run.id, scanned: first, hits: Vec::new(), skipped: 0 };
    loop {
        let page = progress.scanned;
        match run.text(page) {
            Ok(text) if searchable(&text).contains(&run.term) => progress.hits.push(page),
            Ok(_) => {}
            Err(error) if error.is_cancelled() => return,
            Err(_) => progress.skipped += 1,
        }
        progress.scanned += 1;
        let next = progress.scanned;
        if next >= book.len() || next - first >= BATCH || book.kind(next) != PageKind::Text {
            break;
        }
    }
    if run.cancel.is_cancelled() {
        return;
    }
    let next = progress.scanned;
    (run.shared.report)(progress);
    if next < book.len() {
        spawn_step(run, next);
    }
}

impl Run {
    /// The text of `page`: the paragraphs of a text page, OCR text read
    /// before, the book's search text or else the page's text layer, or OCR
    /// of the decoded page.
    fn text(&self, page: usize) -> Result<String> {
        let book = &self.shared.book;
        if book.kind(page) == PageKind::Text
            && let Some(paragraphs) = book.page_text(page)
        {
            return Ok(paragraphs.join("\n"));
        }
        if let Some(text) = self.shared.ocr.lock().unwrap_or_else(PoisonError::into_inner).get(&page) {
            return Ok(text.clone());
        }
        match book.search_text(page, &self.cancel)? {
            Some(text) if !text.trim().is_empty() => return Ok(text),
            Some(_) => {}
            // A layer that fails to load leaves the page to OCR.
            None => match book.text_layer(page, &self.cancel) {
                Ok(Some(layer)) => {
                    let text = layer.lines.iter().map(|line| line.text.as_str()).collect::<Vec<_>>().join("\n");
                    if !text.trim().is_empty() {
                        return Ok(text);
                    }
                }
                Err(error) if error.is_cancelled() => return Err(error),
                _ => {}
            },
        }
        if !tesseract_available() {
            return Err(Error::unsupported("OCR needs tesseract on PATH"));
        }
        let text = recognize_in(&*book.page(page, &self.cancel)?, &self.cancel, &self.tesseract)?;
        self.shared.ocr.lock().unwrap_or_else(PoisonError::into_inner).insert(page, text.clone());
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use crossbeam_channel::{Receiver, unbounded};
    use yoyomi_core::{Pixels, RgbaImage, TextLayer, TextLine};

    use super::*;

    /// A book whose pages are text pages, rendered pages with a text layer,
    /// or images: a picture or, without one, a page that fails to decode.
    struct Fake {
        pages: Vec<(PageKind, String)>,
        picture: Option<Pixels>,
        decodes: AtomicUsize,
        /// Reading a text layer waits for its cancel token and reports here
        /// when it starts.
        hold: Option<crossbeam_channel::Sender<()>>,
        /// The search text of each page, which the book reads apart from its
        /// text layers.
        search_text: Option<Vec<String>>,
    }

    impl Fake {
        fn new(pages: Vec<(PageKind, String)>) -> Self {
            Fake { pages, picture: None, decodes: AtomicUsize::new(0), hold: None, search_text: None }
        }
    }

    impl Book for Fake {
        fn path(&self) -> &Path {
            Path::new("/books/fake")
        }
        fn title(&self) -> &str {
            "fake"
        }
        fn len(&self) -> usize {
            self.pages.len()
        }
        fn kind(&self, page: usize) -> PageKind {
            self.pages[page].0
        }
        fn page(&self, _page: usize, cancel: &Cancel) -> Result<Pixels> {
            self.decodes.fetch_add(1, Ordering::Relaxed);
            cancel.check()?;
            self.picture.clone().ok_or_else(|| Error::Decode("corrupt".into()))
        }
        fn text_layer(&self, page: usize, cancel: &Cancel) -> Result<Option<Arc<TextLayer>>> {
            if let Some(started) = &self.hold {
                let _ = started.send(());
                while !cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            cancel.check()?;
            let (kind, text) = &self.pages[page];
            let line = |text: &str| TextLine { text: text.into(), top: 0.0, bottom: 0.1, end: true, boxes: Vec::new(), links: Vec::new() };
            Ok((*kind == PageKind::Rendered).then(|| Arc::new(TextLayer { lines: text.lines().map(line).collect() })))
        }
        fn search_text(&self, page: usize, cancel: &Cancel) -> Result<Option<String>> {
            cancel.check()?;
            Ok(self.search_text.as_ref().map(|pages| pages[page].clone()))
        }
        fn page_text(&self, page: usize) -> Option<Vec<String>> {
            let (kind, text) = &self.pages[page];
            (*kind == PageKind::Text).then(|| text.lines().map(str::to_owned).collect())
        }
    }

    fn search(book: Fake) -> (Search, Receiver<Progress>) {
        let (sent, received) = unbounded();
        let search = Search::new(Arc::new(book), Jobs::new(2), move |progress| sent.send(progress).unwrap());
        (search, received)
    }

    fn next(received: &Receiver<Progress>) -> Progress {
        received.recv_timeout(Duration::from_secs(60)).expect("a search step")
    }

    #[test]
    fn searchable_folds_case_and_joins_whitespace() {
        assert_eq!(searchable("  HELLO\n\t Reader  "), "hello reader");
        assert_eq!(searchable(""), "");
    }

    #[test]
    fn text_pages_scan_in_batches() {
        // 70 text pages, an image that fails to decode, then the same 70 again.
        let text = |i: usize| (PageKind::Text, if i == 63 || i == 64 { format!("Page {i}\nneedle") } else { format!("Page {i}") });
        let mut pages: Vec<_> = (0..70).map(text).collect();
        pages.push((PageKind::Image, String::new()));
        pages.extend((0..70).map(text));
        let (mut search, received) = search(Fake::new(pages));
        search.start("Needle");
        assert_eq!(search.term(), "needle");
        let mut step = |scanned: usize, hits: &[usize], skipped: usize, done: bool| {
            assert_eq!(search.on_progress(next(&received)), done);
            assert_eq!((search.scanned(), search.hits(), search.skipped()), (scanned, hits, skipped));
            assert_eq!(search.running(), !done);
        };
        // Pages 0-63 in one step, 64-69 up to the image in the next.
        step(64, &[63], 0, false);
        step(70, &[63, 64], 0, false);
        // The image is a step of its own, and the text after it joins that step.
        step(134, &[63, 64], 1, false);
        step(141, &[63, 64, 134, 135], 1, true);
    }

    #[test]
    fn text_layers_match_across_lines() {
        let pages = vec![
            (PageKind::Rendered, "say Hello\nREADER".to_owned()),
            (PageKind::Rendered, "hello   reader".to_owned()),
            (PageKind::Rendered, "hello there".to_owned()),
        ];
        let (mut search, received) = search(Fake::new(pages));
        search.start("HELLO  reader");
        while !search.on_progress(next(&received)) {}
        assert_eq!((search.hits(), search.skipped()), (&[0, 1][..], 0));
    }

    #[test]
    fn search_text_replaces_the_text_layers() {
        let pages = vec![(PageKind::Rendered, "layer needle".to_owned()), (PageKind::Rendered, "layer only".to_owned())];
        let book = Fake { search_text: Some(vec!["say NEEDLE".into(), " ".into()]), ..Fake::new(pages) };
        let (mut search, received) = search(book);
        search.start("needle");
        while !search.on_progress(next(&received)) {}
        // The blank page goes to OCR, which fails on a page without pixels.
        assert_eq!((search.hits(), search.skipped()), (&[0][..], 1));
        search.start("layer");
        while !search.on_progress(next(&received)) {}
        assert_eq!(search.hits(), []);
    }

    #[test]
    fn without_search_text_the_layers_answer_before_ocr() {
        let pages = vec![(PageKind::Rendered, "layer needle".to_owned()), (PageKind::Rendered, "layer only".to_owned())];
        let book = Arc::new(Fake::new(pages));
        let (sent, received) = unbounded();
        let mut search = Search::new(book.clone(), Jobs::new(2), move |progress| sent.send(progress).unwrap());
        search.start("needle");
        while !search.on_progress(next(&received)) {}
        assert_eq!((search.hits(), search.skipped()), (&[0][..], 0));
        assert_eq!(book.decodes.load(Ordering::Relaxed), 0, "no page went to OCR");
    }

    #[test]
    fn hits_wrap_around_the_book() {
        let (mut search, _) = search(Fake::new(Vec::new()));
        search.hits = vec![2, 5, 9];
        assert_eq!(search.hit_from(0, true), Some(2));
        assert_eq!(search.hit_from(5, true), Some(9));
        assert_eq!(search.hit_from(9, true), Some(2));
        assert_eq!(search.hit_from(5, false), Some(2));
        assert_eq!(search.hit_from(2, false), Some(9));
        search.hits.clear();
        assert_eq!(search.hit_from(0, true), None);
    }

    #[test]
    fn progress_counts_only_for_its_own_search() {
        let pages = vec![(PageKind::Text, "needle".to_owned())];
        let (mut search, received) = search(Fake::new(pages.clone()));
        let (mut other, from_other) = self::search(Fake::new(pages));
        search.start("needle");
        let stale = next(&received);
        search.start("needle");
        assert!(!search.on_progress(stale), "a restarted search drops the old steps");
        other.start("needle");
        assert!(!search.on_progress(next(&from_other)), "another book's search never counts");
        assert!(search.on_progress(next(&received)));
        assert_eq!(search.hits(), [0]);
        search.cancel();
        assert!(search.term().is_empty() && search.hits().is_empty() && !search.running());
    }

    #[test]
    fn dropping_the_search_stops_its_steps() {
        let (started, reading) = unbounded();
        let book = Fake { hold: Some(started), ..Fake::new(vec![(PageKind::Image, String::new()); 2]) };
        let (mut search, received) = search(book);
        search.start("needle");
        reading.recv_timeout(Duration::from_secs(5)).expect("the first page starts");
        drop(search);
        assert!(received.recv_timeout(Duration::from_millis(200)).is_err(), "a cancelled step reports nothing");
        assert!(reading.recv_timeout(Duration::from_millis(200)).is_err(), "no step follows a cancelled one");
    }

    /// Black capitals on white, drawn from 5 x 7 glyphs scaled `scale` times.
    fn lettering(word: &str, scale: u32) -> RgbaImage {
        let glyph = |c| match c {
            'H' => ["X...X", "X...X", "X...X", "XXXXX", "X...X", "X...X", "X...X"],
            'E' => ["XXXXX", "X....", "X....", "XXXX.", "X....", "X....", "XXXXX"],
            'L' => ["X....", "X....", "X....", "X....", "X....", "X....", "XXXXX"],
            'O' => [".XXX.", "X...X", "X...X", "X...X", "X...X", "X...X", ".XXX."],
            _ => panic!("no glyph for {c}"),
        };
        let margin = 8 * scale;
        let width = 2 * margin + word.len() as u32 * 7 * scale;
        let mut image = RgbaImage::from_pixel(width, 2 * margin + 7 * scale, [255, 255, 255, 255].into());
        for (i, c) in word.chars().enumerate() {
            for (row, line) in glyph(c).iter().enumerate() {
                for (col, _) in line.chars().enumerate().filter(|&(_, dot)| dot == 'X') {
                    let (x, y) = (margin + (i as u32 * 7 + col as u32) * scale, margin + row as u32 * scale);
                    for (dx, dy) in (0..scale).flat_map(|dx| (0..scale).map(move |dy| (dx, dy))) {
                        image.put_pixel(x + dx, y + dy, [0, 0, 0, 255].into());
                    }
                }
            }
        }
        image
    }

    #[test]
    fn cancel_and_drop_kill_and_reap_tesseract() {
        if !tesseract_available() {
            eprintln!("tesseract is not on PATH; OCR goes untested");
            return;
        }
        for drop_it in [false, true] {
            let book = Fake { picture: Some(Arc::new(lettering("HELLO", 40))), ..Fake::new(vec![(PageKind::Image, String::new())]) };
            let (mut search, _received) = search(book);
            search.start("hello");
            let tesseract = search.tesseract.clone();
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let pid = loop {
                if let Some(child) = &*tesseract.lock().unwrap() {
                    break child.id();
                }
                assert!(std::time::Instant::now() < deadline, "tesseract starts");
                std::thread::sleep(Duration::from_millis(1));
            };
            if drop_it {
                drop(search);
            } else {
                search.cancel();
            }
            // A reaped child leaves no /proc entry; a zombie would.
            assert!(!Path::new(&format!("/proc/{pid}")).exists(), "tesseract {pid} outlived the cancel");
        }
    }

    #[test]
    fn ocr_reads_image_pages_once_per_book() {
        if !tesseract_available() {
            eprintln!("tesseract is not on PATH; OCR goes untested");
            return;
        }
        let book = Arc::new(Fake { picture: Some(Arc::new(lettering("HELLO", 8))), ..Fake::new(vec![(PageKind::Image, String::new())]) });
        let (sent, received) = unbounded();
        let mut search = Search::new(book.clone(), Jobs::new(1), move |progress| sent.send(progress).unwrap());
        for _ in 0..2 {
            search.start("hello");
            assert!(search.on_progress(next(&received)));
            assert_eq!((search.hits(), search.skipped()), (&[0][..], 0));
        }
        assert_eq!(book.decodes.load(Ordering::Relaxed), 1, "the second search reads the cached text");
    }
}
