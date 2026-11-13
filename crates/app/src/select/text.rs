//! Hit testing on a text layer: the link under a point and the text between
//! two points, in page fractions.

use yoyomi_core::{Link, PageRect, TextLayer, TextLine};

/// The line whose box holds a point, else the nearest; ties go to the
/// earlier line in reading order. Lines without word boxes span the page.
fn line_at(lines: &[TextLine], x: f64, y: f64) -> usize {
    let distance = |line: &TextLine| {
        let (x0, x1) = span(line, 0, usize::MAX).unwrap_or((x, x));
        let dx = (x0 - x).max(x - x1).max(0.0);
        let dy = (line.top - y).max(y - line.bottom).max(0.0);
        dx.hypot(dy)
    };
    (0..lines.len()).min_by(|&a, &b| distance(&lines[a]).total_cmp(&distance(&lines[b]))).unwrap_or(0)
}

/// The horizontal extent of the boxes that cover runes `from..to` of `line`.
fn span(line: &TextLine, from: usize, to: usize) -> Option<(f64, f64)> {
    line.boxes
        .iter()
        .filter(|b| b.index < to && b.index + b.count > from)
        .fold(None, |extent, b| Some(extent.map_or((b.x0, b.x1), |(x0, x1): (f64, f64)| (x0.min(b.x0), x1.max(b.x1)))))
}

/// The rune boundary nearest to a point: a line and a rune index in its text.
fn caret_at(lines: &[TextLine], x: f64, y: f64) -> (usize, usize) {
    let n = line_at(lines, x, y);
    let (mut index, mut best) = (0, f64::INFINITY);
    for b in &lines[n].boxes {
        let distance = (b.x0 - x).max(x - b.x1).max(0.0);
        if distance >= best {
            continue;
        }
        best = distance;
        let after = (x > (b.x0 + b.x1) / 2.0) != b.rtl;
        index = if after { b.index + b.count } else { b.index };
    }
    (n, index)
}

/// The link under a point of the page.
pub(crate) fn link_at(layer: &TextLayer, x: f64, y: f64) -> Option<&Link> {
    let lines = &layer.lines;
    if lines.is_empty() {
        return None;
    }
    let line = &lines[line_at(lines, x, y)];
    if y < line.top || y > line.bottom {
        return None;
    }
    let index = line.boxes.iter().rev().find(|b| x >= b.x0 && x < b.x1)?.index;
    line.links.iter().find(|link| (link.start..link.end).contains(&index)).map(|link| &link.target)
}

/// The text between two points of the page in reading order, and the
/// rectangles it covers, one per line.
pub(crate) fn select(layer: &TextLayer, from: (f64, f64), to: (f64, f64)) -> (String, Vec<PageRect>) {
    let lines = &layer.lines;
    if lines.is_empty() {
        return (String::new(), Vec::new());
    }
    let (mut from, mut to) = (caret_at(lines, from.0, from.1), caret_at(lines, to.0, to.1));
    if to < from {
        (from, to) = (to, from);
    }
    let mut text = String::new();
    let mut rects = Vec::new();
    for (n, line) in lines.iter().enumerate().take(to.0 + 1).skip(from.0) {
        let runes: Vec<char> = line.text.chars().collect();
        let lo = if n == from.0 { from.1 } else { 0 };
        let hi = if n == to.0 { to.1.min(runes.len()) } else { runes.len() };
        if lo >= hi {
            continue;
        }
        text.extend(&runes[lo..hi]);
        if line.end && n < to.0 {
            text.push('\n');
        }
        if let Some((x0, x1)) = span(line, lo, hi) {
            rects.push(PageRect { x0, y0: line.top, x1, y1: line.bottom });
        }
    }
    (text.trim_matches(' ').to_owned(), rects)
}

#[cfg(test)]
pub(crate) mod tests {
    use yoyomi_core::{LinkSpan, TextBox};

    use super::*;

    /// A line of words, each `0.1` wide with a `0.05` gap, from `x = 0.1`.
    pub(crate) fn line(words: &[&str], top: f64, end: bool, links: Vec<LinkSpan>) -> TextLine {
        let mut text = words.join(" ");
        let mut boxes = Vec::new();
        let mut index = 0;
        for (i, word) in words.iter().enumerate() {
            let x0 = 0.1 + i as f64 * 0.15;
            boxes.push(TextBox { x0, x1: x0 + 0.1, index, count: word.chars().count(), rtl: false });
            index += word.chars().count() + 1;
        }
        if !end {
            text.push(' ');
        }
        TextLine { text, top, bottom: top + 0.1, end, boxes, links }
    }

    /// A page like the PDF fixture of the formats crate: a web link over
    /// "visit example" and a page link over "next page".
    pub(crate) fn page() -> TextLayer {
        let url = Link::Url("https://example.com/a?utm_source=pdf".into());
        TextLayer {
            lines: vec![
                line(&["Hello", "reader"], 0.1, true, Vec::new()),
                line(&["visit", "example"], 0.3, true, vec![LinkSpan { start: 0, end: 13, target: url }]),
                line(&["next", "page"], 0.5, true, vec![LinkSpan { start: 0, end: 9, target: Link::Page(1) }]),
            ],
        }
    }

    fn middle(layer: &TextLayer, line: usize, word: usize) -> (f64, f64) {
        let (line, b) = (&layer.lines[line], layer.lines[line].boxes[word]);
        ((b.x0 + b.x1) / 2.0, (line.top + line.bottom) / 2.0)
    }

    #[test]
    fn links_lie_under_their_words() {
        let layer = page();
        let (x, y) = middle(&layer, 1, 0);
        assert_eq!(link_at(&layer, x, y), Some(&Link::Url("https://example.com/a?utm_source=pdf".into())));
        let (x, y) = middle(&layer, 2, 1);
        assert_eq!(link_at(&layer, x, y), Some(&Link::Page(1)));
        let (x, y) = middle(&layer, 0, 0);
        assert_eq!(link_at(&layer, x, y), None, "plain words are not links");
        assert_eq!(link_at(&layer, 0.15, 0.25), None, "between lines");
        assert_eq!(link_at(&TextLayer::default(), 0.5, 0.5), None);
    }

    #[test]
    fn selections_run_in_reading_order() {
        let layer = page();
        let (text, rects) = select(&layer, (0.0, 0.0), (1.0, 1.0));
        assert_eq!(text, "Hello reader\nvisit example\nnext page");
        assert_eq!(rects.len(), 3);
        let first = &layer.lines[0];
        assert_eq!(rects[0], PageRect { x0: first.boxes[0].x0, y0: first.top, x1: first.boxes[1].x1, y1: first.bottom });
        let (x, y) = middle(&layer, 0, 1);
        assert_eq!(select(&layer, (x - 0.05, y), (1.0, 1.0)).0, "reader\nvisit example\nnext page");
        assert_eq!(select(&layer, (1.0, 1.0), (x - 0.05, y)).0, "reader\nvisit example\nnext page", "a drag upward");
        assert_eq!(select(&layer, (x, y), (x, y)).0, "", "a click selects nothing");
        let paragraph = TextLayer { lines: vec![line(&["one", "two"], 0.1, false, Vec::new()), line(&["three"], 0.3, true, Vec::new())] };
        assert_eq!(select(&paragraph, (0.0, 0.0), (1.0, 1.0)).0, "one two three", "lines of a paragraph join with spaces");
    }

    #[test]
    fn points_pick_lines_of_their_own_column() {
        // The right column sits half a line lower, so its lines end below the left column's.
        let right = |words: &[&str], top: f64, links: Vec<LinkSpan>| {
            let mut line = line(words, top, true, links);
            line.boxes.iter_mut().for_each(|b| (b.x0, b.x1) = (b.x0 + 0.5, b.x1 + 0.5));
            line
        };
        let layer = TextLayer {
            lines: vec![
                line(&["a1", "a2"], 0.1, true, Vec::new()),
                line(&["b1", "b2"], 0.3, true, Vec::new()),
                right(&["c1", "c2"], 0.15, vec![LinkSpan { start: 0, end: 5, target: Link::Page(3) }]),
                right(&["d1", "d2"], 0.35, Vec::new()),
            ],
        };
        let (x, y) = middle(&layer, 2, 0);
        assert_eq!(link_at(&layer, x, y), Some(&Link::Page(3)), "a link in the right column");
        assert_eq!(link_at(&layer, middle(&layer, 0, 0).0, y), None, "the same height in the left column");
        let (end_x, end_y) = middle(&layer, 3, 1);
        assert_eq!(select(&layer, (x - 0.05, y), (end_x + 0.05, end_y)).0, "c1 c2\nd1 d2", "a drag in the right column");
        assert_eq!(select(&layer, (0.0, 0.0), (1.0, 1.0)).0, "a1 a2\nb1 b2\nc1 c2\nd1 d2", "columns keep the layer's order");
    }
}
