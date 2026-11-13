//! The native macOS reader: a fullscreen AppKit window that shows the book
//! of the terminal's active pane through the same pager and strip, and hands
//! the reading state back when it closes. Other platforms build a stub.

#[cfg(target_os = "macos")]
mod macos;
#[cfg_attr(not(target_os = "macos"), allow(dead_code, reason = "only the macOS window and the tests use it"))]
mod pixels;
#[cfg_attr(not(target_os = "macos"), allow(dead_code, reason = "only the macOS window and the tests use it"))]
mod reader;

use std::sync::Arc;

use yoyomi_core::{Book, Jobs, Link, PageCache, Result};

/// The reading state the terminal and the window hand each other.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub page: usize,
    /// In webtoon mode, the fraction of `page` above the top of the view.
    pub offset: f64,
    pub webtoon: bool,
    pub quarter_turns: u8,
    pub zoom: f64,
    pub center: (f64, f64),
    pub spread: bool,
    pub invert: bool,
}

impl Default for State {
    fn default() -> Self {
        State { page: 0, offset: 0.0, webtoon: false, quarter_turns: 0, zoom: 1.0, center: (0.5, 0.5), spread: false, invert: false }
    }
}

/// How the window closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `q` or the close button: forget the saved positions and quit.
    Clear,
    /// `Q`: save the positions and quit.
    Save,
    /// `f`: go back to the terminal.
    Return,
}

/// The state the window closed with and what to do next.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub state: State,
    pub action: Action,
    /// Why the last frame failed, if it did.
    pub error: Option<String>,
}

/// Finds the link at `x`, `y` of `page`, in fractions of the unturned page.
/// It runs on a job thread and may read the page's text layer.
pub type LinkAt = fn(&dyn Book, usize, f64, f64) -> Option<Link>;

/// Whether this build has the native window.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// Shows `book` from `state` in a fullscreen window until a key or the close
/// button ends it. Runs on the main thread and blocks it; pages render on
/// `jobs`. Clicks follow the links `link_at` finds.
#[cfg(target_os = "macos")]
pub fn run(book: Arc<PageCache>, jobs: Jobs, state: State, link_at: LinkAt) -> Result<Outcome> {
    macos::run(book, jobs, state, link_at)
}

/// Reports that this build has no native window.
#[cfg(not(target_os = "macos"))]
pub fn run(book: Arc<PageCache>, jobs: Jobs, state: State, link_at: LinkAt) -> Result<Outcome> {
    let _ = (book, jobs, state, link_at);
    Err(yoyomi_core::Error::unsupported("the native reader needs macOS"))
}
