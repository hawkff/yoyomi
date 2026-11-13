use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, Result};

/// A cancellation flag shared between the code that asks for work and the
/// code doing it. Clones share one flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A token nobody has cancelled yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops the work holding a clone of this token at its next check.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Returns `Error::Cancelled` once the token is cancelled.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_the_flag() {
        let token = Cancel::new();
        let clone = token.clone();
        assert!(clone.check().is_ok());
        token.cancel();
        assert!(clone.is_cancelled());
        assert!(clone.check().unwrap_err().is_cancelled());
    }
}
