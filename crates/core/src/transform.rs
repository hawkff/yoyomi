use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// A rectangle in fractions of a page's width and height.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PageRect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl PageRect {
    fn bits(&self) -> [u64; 4] {
        [self.x0.to_bits(), self.y0.to_bits(), self.x1.to_bits(), self.y1.to_bits()]
    }
}

/// How a page turns, zooms and changes color before a renderer draws it.
///
/// Equality and hashing compare the floats bit for bit, so a transform can
/// key a cache.
#[derive(Clone, Debug)]
pub struct Transform {
    /// Clockwise quarter turns, 0 through 3.
    pub quarter_turns: u8,
    /// 1 shows the whole page; 2 shows half of each side, and so on.
    pub zoom: f64,
    /// The center of the view, as fractions of the turned page.
    pub center: (f64, f64),
    /// Inverts the colors where the book allows it.
    pub invert: bool,
    /// Text selection highlights, as fractions of the unturned page.
    pub highlights: Arc<[PageRect]>,
}

impl Default for Transform {
    fn default() -> Self {
        Transform { quarter_turns: 0, zoom: 1.0, center: (0.5, 0.5), invert: false, highlights: Vec::new().into() }
    }
}

impl Transform {
    /// Whether the page shows whole, without a zoom crop.
    pub fn is_whole(&self) -> bool {
        self.zoom <= 1.001
    }

    fn bits(&self) -> (u8, u64, u64, u64, bool) {
        (self.quarter_turns & 3, self.zoom.to_bits(), self.center.0.to_bits(), self.center.1.to_bits(), self.invert)
    }
}

impl PartialEq for Transform {
    fn eq(&self, other: &Self) -> bool {
        self.bits() == other.bits()
            && self.highlights.len() == other.highlights.len()
            && self.highlights.iter().zip(other.highlights.iter()).all(|(a, b)| a.bits() == b.bits())
    }
}

impl Eq for Transform {}

impl Hash for Transform {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bits().hash(state);
        for rect in self.highlights.iter() {
            rect.bits().hash(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn equal_transforms_key_one_entry() {
        let a = Transform { zoom: 1.25, highlights: vec![PageRect { x0: 0.1, y0: 0.2, x1: 0.3, y1: 0.4 }].into(), ..Transform::default() };
        let b = Transform { highlights: vec![PageRect { x0: 0.1, y0: 0.2, x1: 0.3, y1: 0.4 }].into(), ..a.clone() };
        let keys: HashSet<_> = [a.clone(), b, Transform::default()].into_iter().collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(a, Transform { quarter_turns: 1, ..a.clone() });
    }

    #[test]
    fn quarter_turns_wrap() {
        assert_eq!(Transform { quarter_turns: 5, ..Transform::default() }, Transform { quarter_turns: 1, ..Transform::default() });
    }
}
