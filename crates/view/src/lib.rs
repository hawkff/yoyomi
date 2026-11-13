//! The pipeline between books and the terminal. The pager shows pages and
//! readies the next one before the reader turns to it. The strip shows a
//! webtoon strip and keeps its pieces in the terminal while the reader
//! scrolls. Both produce scenes and uploads, and only the app writes to the
//! terminal.

mod pager;
mod resident;
mod strip;

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

pub use pager::{PageRequest, Pager};
pub use resident::{Lease, Resident};
pub use strip::{Strip, StripPosition, StripRequest};
use yoyomi_core::{Scene, Upload};

/// A frame ready to show.
#[derive(Clone, Debug, Default)]
pub struct Shown {
    /// The page at the top, or on the left of a spread.
    pub page: usize,
    pub scene: Scene,
    /// Messages for pages that failed; the scene leaves their place empty.
    pub errors: Vec<String>,
}

/// What a request or a finished job changes.
#[derive(Clone, Debug, Default)]
pub struct Update {
    /// Kitty images to send now, before any frame that shows them. Under
    /// tmux, only an update with a frame has uploads: those of every image
    /// the frame draws.
    pub uploads: Vec<Upload>,
    /// A new frame for the pane, when the work answered its latest request.
    pub shown: Option<Shown>,
}

/// A finished background job of a pager or a strip. The app hands every
/// event to its pagers and its strip in turn; each takes the events it
/// started and hands back the rest.
pub struct ViewEvent {
    source: u64,
    payload: Box<dyn Any + Send>,
}

impl ViewEvent {
    /// An event for the pager or strip that `source` names.
    pub(crate) fn new(source: u64, payload: impl Any + Send) -> Self {
        ViewEvent { source, payload: Box::new(payload) }
    }

    /// The payload, when `source` started this event and it holds a `T`;
    /// otherwise the event itself, for the next owner to try.
    pub(crate) fn take<T: Any>(self, source: u64) -> Result<T, ViewEvent> {
        if self.source != source {
            return Err(self);
        }
        match self.payload.downcast::<T>() {
            Ok(payload) => Ok(*payload),
            Err(payload) => Err(ViewEvent { source: self.source, payload }),
        }
    }
}

impl fmt::Debug for ViewEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewEvent").field("source", &self.source).finish_non_exhaustive()
    }
}

/// A new event source id for a pager or a strip.
pub(crate) fn next_source() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_go_back_to_their_source() {
        let (mine, other) = (next_source(), next_source());
        let event = ViewEvent::new(mine, 42_u32);
        let event = event.take::<u32>(other).unwrap_err();
        let event = event.take::<String>(mine).unwrap_err();
        assert_eq!(event.take::<u32>(mine).unwrap(), 42);
    }
}
