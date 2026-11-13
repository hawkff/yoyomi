use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::{Book, Cancel, Chapter, Encoded, Error, MAX_PAGE_PIXELS, PageKind, Pixels, Result, TextLayer};

/// Decoded pages of one book, shared by every view of it.
///
/// Concurrent requests for one page share a single decode, and its failure:
/// a slow converter that fails does not run again for each waiting request.
/// A cancelled decode lets the next request try again. The cache keeps the
/// most recently used `MAX_PAGES` pages within `MAX_PIXELS`. A `PageCache`
/// is a `Book` itself, so views take it wherever a book goes. After `close`,
/// every page request fails with `Error::Cancelled`.
pub struct PageCache {
    book: Arc<dyn Book>,
    state: Mutex<State>,
    done: Condvar,
}

#[derive(Default)]
struct State {
    pages: HashMap<usize, Pixels>,
    /// Cached pages, least recently used first.
    order: VecDeque<usize>,
    pixels: u64,
    /// Pages being decoded, with the attempt that decodes each.
    loading: HashMap<usize, u64>,
    /// The last failed attempt of a page, for the requests that waited on it.
    /// A new attempt keeps it, so a waiter that wakes late still finds it.
    // shortcut: a waiter that wakes after a second attempt also failed tries
    // a third time; results kept per attempt would cover that rare order.
    failed: HashMap<usize, (u64, Error)>,
    attempts: u64,
    closed: bool,
}

impl PageCache {
    pub const MAX_PAGES: usize = 8;
    pub const MAX_PIXELS: u64 = 2 * MAX_PAGE_PIXELS;

    pub fn new(book: Arc<dyn Book>) -> Self {
        PageCache { book, state: Mutex::new(State::default()), done: Condvar::new() }
    }

    /// The book behind the cache.
    pub fn inner(&self) -> &Arc<dyn Book> {
        &self.book
    }

    /// Page `page` when the cache holds it, without decoding.
    pub fn cached(&self, page: usize) -> Option<Pixels> {
        self.state.lock().pages.get(&page).cloned()
    }

    /// Whether the cache can hold the pages in `shown` together with as many
    /// pixels again for the pages after them. Pages too large for that would
    /// push the shown ones out, and every pan or zoom would decode them again.
    pub fn room_to_preload(&self, shown: &[usize]) -> bool {
        let state = self.state.lock();
        let pixels: u64 = shown.iter().filter_map(|page| state.pages.get(page)).map(pixel_count).sum();
        2 * pixels <= Self::MAX_PIXELS
    }

    fn load(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
        let mut state = self.state.lock();
        let mut waited = None;
        let attempt = loop {
            cancel.check()?;
            if state.closed {
                return Err(Error::Cancelled);
            }
            if let Some(pixels) = state.pages.get(&page).cloned() {
                state.touch(page);
                return Ok(pixels);
            }
            if let (Some(attempt), Some((failed, error))) = (waited, state.failed.get(&page))
                && *failed == attempt
            {
                return Err(error.clone());
            }
            let decoding = state.loading.get(&page).copied();
            match decoding {
                // Another request decodes this page. Wake up now and then to
                // notice a cancelled request.
                Some(attempt) => {
                    waited = Some(attempt);
                    self.done.wait_for(&mut state, Duration::from_millis(50));
                }
                None => {
                    state.attempts += 1;
                    let attempt = state.attempts;
                    state.loading.insert(page, attempt);
                    break attempt;
                }
            }
        };
        drop(state);
        let _loading = Loading { cache: self, page };
        let result = self.book.page(page, cancel);
        let mut state = self.state.lock();
        // A decode that outlived `close` neither refills the cache nor reports
        // a failure of the closed book.
        if state.closed {
            return Err(Error::Cancelled);
        }
        match result {
            Ok(pixels) => {
                state.failed.remove(&page);
                state.insert(page, pixels.clone());
                Ok(pixels)
            }
            Err(error) => {
                if !error.is_cancelled() {
                    state.failed.insert(page, (attempt, error.clone()));
                }
                Err(error)
            }
        }
    }
}

/// Clears a page's loading mark however its decode ends, then wakes the
/// threads waiting for it.
struct Loading<'a> {
    cache: &'a PageCache,
    page: usize,
}

impl Drop for Loading<'_> {
    fn drop(&mut self) {
        self.cache.state.lock().loading.remove(&self.page);
        self.cache.done.notify_all();
    }
}

impl State {
    fn touch(&mut self, page: usize) {
        if let Some(i) = self.order.iter().position(|&p| p == page) {
            self.order.remove(i);
            self.order.push_back(page);
        }
    }

    fn insert(&mut self, page: usize, pixels: Pixels) {
        if self.pages.contains_key(&page) {
            self.touch(page);
            return;
        }
        self.pixels += pixel_count(&pixels);
        self.pages.insert(page, pixels);
        self.order.push_back(page);
        // Large scans take hundreds of megabytes each, so the pixel total is
        // bounded as well as the page count. The newest page always stays.
        while self.order.len() > PageCache::MAX_PAGES || (self.pixels > PageCache::MAX_PIXELS && self.order.len() > 1) {
            let Some(oldest) = self.order.pop_front() else { break };
            if let Some(evicted) = self.pages.remove(&oldest) {
                self.pixels -= pixel_count(&evicted);
            }
        }
    }
}

fn pixel_count(pixels: &Pixels) -> u64 {
    u64::from(pixels.width()) * u64::from(pixels.height())
}

impl Book for PageCache {
    fn path(&self) -> &Path {
        self.book.path()
    }

    fn title(&self) -> &str {
        self.book.title()
    }

    fn len(&self) -> usize {
        self.book.len()
    }

    fn kind(&self, page: usize) -> PageKind {
        self.book.kind(page)
    }

    fn can_invert(&self, page: usize) -> bool {
        self.book.can_invert(page)
    }

    fn page(&self, page: usize, cancel: &Cancel) -> Result<Pixels> {
        self.load(page, cancel)
    }

    fn page_size(&self, page: usize, cancel: &Cancel) -> Result<(u32, u32)> {
        cancel.check()?;
        let cached = {
            let state = self.state.lock();
            if state.closed {
                return Err(Error::Cancelled);
            }
            state.pages.get(&page).map(|pixels| pixels.dimensions())
        };
        if let Some(size) = cached {
            return Ok(size);
        }
        let size = self.book.page_size(page, cancel)?;
        if self.state.lock().closed {
            return Err(Error::Cancelled);
        }
        Ok(size)
    }

    fn chapters(&self, cancel: &Cancel) -> Result<Vec<Chapter>> {
        self.book.chapters(cancel)
    }

    fn text_layer(&self, page: usize, cancel: &Cancel) -> Result<Option<Arc<TextLayer>>> {
        self.book.text_layer(page, cancel)
    }

    fn search_text(&self, page: usize, cancel: &Cancel) -> Result<Option<String>> {
        self.book.search_text(page, cancel)
    }

    fn page_text(&self, page: usize) -> Option<Vec<String>> {
        self.book.page_text(page)
    }

    /// The book's bytes of `page`, converted from the cached pixels when the
    /// cache holds the page.
    fn encoded(&self, page: usize, cancel: &Cancel) -> Result<Encoded> {
        match self.cached(page) {
            Some(pixels) => self.book.encoded_from(page, &pixels, cancel),
            None => self.book.encoded(page, cancel),
        }
    }

    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.pages.clear();
        state.order.clear();
        state.failed.clear();
        state.pixels = 0;
        drop(state);
        self.done.notify_all();
        self.book.close();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use image::RgbaImage;

    use super::*;

    /// Pages of `width` x `height` pixels that count their decodes, or fail
    /// to decode.
    struct Counted {
        path: PathBuf,
        width: u32,
        height: u32,
        decodes: AtomicUsize,
        delay: Duration,
        fail: bool,
    }

    impl Counted {
        fn new(width: u32, height: u32, delay: Duration) -> Arc<Self> {
            let path = PathBuf::from("/books/counted.cbz");
            Arc::new(Counted { path, width, height, decodes: AtomicUsize::new(0), delay, fail: false })
        }

        fn failing(delay: Duration) -> Arc<Self> {
            let path = PathBuf::from("/books/broken.pdf");
            Arc::new(Counted { path, width: 1, height: 1, decodes: AtomicUsize::new(0), delay, fail: true })
        }
    }

    impl Book for Counted {
        fn path(&self) -> &Path {
            &self.path
        }

        fn title(&self) -> &str {
            "counted"
        }

        fn len(&self) -> usize {
            20
        }

        fn kind(&self, _page: usize) -> PageKind {
            PageKind::Image
        }

        fn page(&self, _page: usize, cancel: &Cancel) -> Result<Pixels> {
            cancel.check()?;
            self.decodes.fetch_add(1, Ordering::SeqCst);
            thread::sleep(self.delay);
            cancel.check()?;
            if self.fail {
                return Err(Error::Tool { tool: "pdftoppm".into(), message: "broken page".into() });
            }
            Ok(Arc::new(RgbaImage::new(self.width, self.height)))
        }
    }

    #[test]
    fn concurrent_requests_share_one_decode() {
        let book = Counted::new(2, 2, Duration::from_millis(100));
        let cache = Arc::new(PageCache::new(book.clone()));
        let start = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (cache, start) = (cache.clone(), start.clone());
                thread::spawn(move || {
                    start.wait();
                    cache.page(3, &Cancel::new()).unwrap()
                })
            })
            .collect();
        let pages: Vec<Pixels> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(book.decodes.load(Ordering::SeqCst), 1);
        assert!(pages.iter().all(|p| Arc::ptr_eq(p, &pages[0])));
    }

    #[test]
    fn keeps_the_most_recently_used_pages() {
        let book = Counted::new(2, 2, Duration::ZERO);
        let cache = PageCache::new(book.clone());
        let cancel = Cancel::new();
        for page in 0..PageCache::MAX_PAGES {
            cache.page(page, &cancel).unwrap();
        }
        cache.page(0, &cancel).unwrap(); // page 0 becomes the most recent
        cache.page(PageCache::MAX_PAGES, &cancel).unwrap();
        assert!(cache.cached(0).is_some());
        assert!(cache.cached(1).is_none(), "the least recently used page goes first");
        assert_eq!(book.decodes.load(Ordering::SeqCst), PageCache::MAX_PAGES + 1);
    }

    #[test]
    fn bounds_the_pixel_total() {
        // Three 30-megapixel pages exceed the 64-megapixel budget.
        let book = Counted::new(6000, 5000, Duration::ZERO);
        let cache = PageCache::new(book);
        let cancel = Cancel::new();
        for page in 0..3 {
            cache.page(page, &cancel).unwrap();
        }
        assert!(cache.cached(0).is_none());
        assert!(cache.cached(1).is_some() && cache.cached(2).is_some());
        assert!(cache.room_to_preload(&[2]), "one shown page leaves room for one more");
        assert!(!cache.room_to_preload(&[1, 2]), "two shown pages leave no room for two more");
        assert!(cache.room_to_preload(&[19]), "pages the cache does not hold take no room");
    }

    #[test]
    fn a_cancelled_request_does_not_wait() {
        let book = Counted::new(2, 2, Duration::from_millis(300));
        let cache = Arc::new(PageCache::new(book));
        let first = {
            let cache = cache.clone();
            thread::spawn(move || cache.page(0, &Cancel::new()))
        };
        thread::sleep(Duration::from_millis(50));
        let cancel = Cancel::new();
        let waiter = {
            let (cache, cancel) = (cache.clone(), cancel.clone());
            thread::spawn(move || cache.page(0, &cancel))
        };
        thread::sleep(Duration::from_millis(20));
        cancel.cancel();
        assert!(waiter.join().unwrap().unwrap_err().is_cancelled());
        assert!(first.join().unwrap().is_ok());
    }

    #[test]
    fn waiting_requests_share_a_failure() {
        let book = Counted::failing(Duration::from_millis(150));
        let cache = Arc::new(PageCache::new(book.clone()));
        let start = Arc::new(Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (cache, start) = (cache.clone(), start.clone());
                thread::spawn(move || {
                    start.wait();
                    cache.page(0, &Cancel::new())
                })
            })
            .collect();
        for thread in threads {
            assert!(matches!(thread.join().unwrap(), Err(Error::Tool { .. })));
        }
        assert_eq!(book.decodes.load(Ordering::SeqCst), 1, "the waiting requests did not decode again");
        assert!(cache.page(0, &Cancel::new()).is_err());
        assert_eq!(book.decodes.load(Ordering::SeqCst), 2, "a later request tries again");
    }

    #[test]
    fn a_cancelled_decode_lets_the_waiters_try_again() {
        let book = Counted::new(2, 2, Duration::from_millis(150));
        let cache = Arc::new(PageCache::new(book.clone()));
        let leader = Cancel::new();
        let first = {
            let (cache, leader) = (cache.clone(), leader.clone());
            thread::spawn(move || cache.page(0, &leader))
        };
        thread::sleep(Duration::from_millis(30));
        let waiter = {
            let cache = cache.clone();
            thread::spawn(move || cache.page(0, &Cancel::new()))
        };
        thread::sleep(Duration::from_millis(30));
        leader.cancel();
        assert!(first.join().unwrap().unwrap_err().is_cancelled());
        assert!(waiter.join().unwrap().is_ok(), "the waiter decoded the page itself");
        assert_eq!(book.decodes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn page_size_stops_with_its_request() {
        let cache = PageCache::new(Counted::new(4, 3, Duration::ZERO));
        cache.page(0, &Cancel::new()).unwrap();
        assert_eq!(cache.page_size(0, &Cancel::new()).unwrap(), (4, 3));
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(cache.page_size(0, &cancel).unwrap_err().is_cancelled());
        cache.close();
        assert!(cache.page_size(0, &Cancel::new()).unwrap_err().is_cancelled());
    }

    #[test]
    fn a_decode_that_outlives_close_does_not_refill_the_cache() {
        let book = Counted::new(2, 2, Duration::from_millis(200));
        let cache = Arc::new(PageCache::new(book));
        let decode = {
            let cache = cache.clone();
            thread::spawn(move || cache.page(0, &Cancel::new()))
        };
        thread::sleep(Duration::from_millis(50));
        cache.close();
        assert!(decode.join().unwrap().unwrap_err().is_cancelled());
        assert!(cache.cached(0).is_none());
        assert!(cache.page(0, &Cancel::new()).unwrap_err().is_cancelled());
    }
}
