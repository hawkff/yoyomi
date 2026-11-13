//! Where pages sit in the strip, for the pages scrolling has reached so far.

use std::collections::VecDeque;

/// A page's place in the strip, in strip pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placed {
    pub page: usize,
    pub top: i64,
    pub width: u32,
    pub height: u32,
}

impl Placed {
    pub fn bottom(&self) -> i64 {
        self.top + i64::from(self.height)
    }
}

/// Consecutive pages one under another. The anchor page's top sits at strip
/// row 0, and pages above it have negative tops.
pub(super) struct Layout {
    count: usize,
    pages: VecDeque<Placed>,
}

impl Layout {
    /// A layout of a book of `count` pages holding only `anchor`, sized
    /// `size`.
    pub fn new(anchor: usize, count: usize, (width, height): (u32, u32)) -> Self {
        Layout { count, pages: VecDeque::from([Placed { page: anchor, top: 0, width, height }]) }
    }

    /// Places pages above and below until the layout covers strip rows
    /// `y0..y1`, reaches an end of the book or meets a page `size_of` does
    /// not know yet. `size_of` gives a page's scaled size.
    pub fn reach(&mut self, y0: i64, y1: i64, mut size_of: impl FnMut(usize) -> Option<(u32, u32)>) {
        while let Some(&first) = self.pages.front()
            && first.page > 0
            && first.top > y0
            && let Some((width, height)) = size_of(first.page - 1)
        {
            self.pages.push_front(Placed { page: first.page - 1, top: first.top - i64::from(height), width, height });
        }
        while let Some(&last) = self.pages.back()
            && last.page + 1 < self.count
            && last.bottom() < y1
            && let Some((width, height)) = size_of(last.page + 1)
        {
            self.pages.push_back(Placed { page: last.page + 1, top: last.bottom(), width, height });
        }
    }

    /// Whether the layout holds every page in strip rows `y0..y1`.
    pub fn covers(&self, y0: i64, y1: i64) -> bool {
        self.pages
            .front()
            .zip(self.pages.back())
            .is_some_and(|(first, last)| (first.page == 0 || first.top <= y0) && (last.page + 1 == self.count || last.bottom() >= y1))
    }

    /// The top of page 0, once the layout holds it.
    pub fn start(&self) -> Option<i64> {
        self.pages.front().filter(|first| first.page == 0).map(|first| first.top)
    }

    /// The bottom of the last page, once the layout holds it.
    pub fn end(&self) -> Option<i64> {
        self.pages.back().filter(|last| last.page + 1 == self.count).map(Placed::bottom)
    }

    /// The page at strip row `y`: the last one whose top is at or above it.
    pub fn page_at(&self, y: f64) -> Placed {
        let after = self.pages.partition_point(|placed| placed.top as f64 <= y);
        self.pages[after.saturating_sub(1)]
    }

    /// The pages that cover any of strip rows `y0..y1`.
    pub fn overlapping(&self, y0: i64, y1: i64) -> impl Iterator<Item = Placed> + '_ {
        self.pages.iter().copied().filter(move |placed| placed.top < y1 && placed.bottom() > y0)
    }
}
