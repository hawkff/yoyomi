use parking_lot::Mutex;

use crate::Rgb;

/// A kitty image id. Placeholder cells carry it in their foreground color, so
/// it stays below 2^24.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(pub u32);

impl ImageId {
    /// The foreground color that names this image in placeholder cells.
    pub fn color(self) -> Rgb {
        let [_, r, g, b] = self.0.to_be_bytes();
        Rgb(r, g, b)
    }
}

/// Hands out the 255 kitty image ids of this process. The ids start above a
/// base taken from the process id, so tmux panes that share one terminal do
/// not replace each other's images.
pub struct ImageIds {
    base: u32,
    used: Mutex<[u64; 4]>,
}

impl ImageIds {
    /// How many ids one process gets.
    pub const PER_PROCESS: u32 = 255;

    /// The ids of this process.
    pub fn for_process() -> Self {
        Self::with_base((std::process::id() % 0xffff) << 8)
    }

    /// Ids `base + 1` through `base + 255`.
    ///
    /// # Panics
    ///
    /// When the last id would not fit 24 bits.
    pub fn with_base(base: u32) -> Self {
        assert!(base < (1 << 24) - Self::PER_PROCESS, "kitty image ids must fit 24 bits");
        ImageIds { base, used: Mutex::new([0; 4]) }
    }

    /// A free id, or `None` when all 255 are taken.
    pub fn take(&self) -> Option<ImageId> {
        let mut used = self.used.lock();
        let n = (1..=Self::PER_PROCESS).find(|&n| used[(n / 64) as usize] & (1u64 << (n % 64)) == 0)?;
        used[(n / 64) as usize] |= 1u64 << (n % 64);
        Some(ImageId(self.base + n))
    }

    /// Returns `id` to the pool. Ids from another pool are ignored.
    pub fn give_back(&self, id: ImageId) {
        if let Some(n) = id.0.checked_sub(self.base).filter(|n| (1..=Self::PER_PROCESS).contains(n)) {
            self.used.lock()[(n / 64) as usize] &= !(1u64 << (n % 64));
        }
    }

    /// Every id this pool can hand out, for freeing them all at exit.
    pub fn all(&self) -> impl Iterator<Item = ImageId> {
        let base = self.base;
        (1..=Self::PER_PROCESS).map(move |n| ImageId(base + n))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn hands_out_255_distinct_ids() {
        let ids = ImageIds::with_base(0x1200);
        let taken: HashSet<_> = (0..255).map(|_| ids.take().unwrap()).collect();
        assert_eq!(taken.len(), 255);
        assert!(taken.iter().all(|id| id.0 > 0x1200 && id.0 <= 0x12ff));
        assert_eq!(ids.take(), None);
        ids.give_back(ImageId(0x1203));
        assert_eq!(ids.take(), Some(ImageId(0x1203)));
        assert_eq!(ids.all().count(), 255);
    }

    #[test]
    fn the_color_carries_the_id() {
        // 0x1203 is 4611, and its color bytes are 0, 18 and 3.
        assert_eq!(ImageId(0x1203).color(), Rgb(0, 18, 3));
    }

    #[test]
    #[should_panic(expected = "fit 24 bits")]
    fn a_base_too_high_fails_without_overflow() {
        let _ = ImageIds::with_base(u32::MAX);
    }

    #[test]
    fn process_ids_fit_24_bits() {
        let ids = ImageIds::for_process();
        assert!(ids.all().all(|id| id.0 > 0 && id.0 < 1 << 24));
    }
}
