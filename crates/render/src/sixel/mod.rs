//! DEC sixel graphics. A sixel image is plain pixels on the screen, so the
//! app draws it again whenever the cells under it change.

use std::ops::Range;

use parking_lot::Mutex;
use yoyomi_core::{CellSize, Frame, Grid, ImageId, Renderer, RendererKind, Result, RgbaImage, cells_for, fit_box};

use crate::ops::{invert, scale};

/// Draws images as sixel graphics with a palette fitted to the terminal.
pub struct Sixel {
    cell: Mutex<CellSize>,
    colors: u16,
    background: Option<[u8; 3]>,
}

impl Sixel {
    /// A renderer for cells of `cell` pixels and a terminal with `colors`
    /// color registers, at most 256. Below 2 means the terminal did not say,
    /// and sixel terminals that stay silent offer 256.
    pub fn new(cell: CellSize, colors: u16) -> Self {
        let colors = if colors < 2 { 256 } else { colors.min(256) };
        Sixel { cell: Mutex::new(cell), colors, background: None }
    }

    /// Names the terminal's `background` color in each image with unpainted
    /// pixels: iTerm2 fills those with the highest register the image paints
    /// with, never one below register 2.
    #[must_use]
    pub fn with_background(mut self, background: Option<[u8; 3]>) -> Self {
        self.background = background;
        self
    }
}

impl Renderer for Sixel {
    fn kind(&self) -> RendererKind {
        RendererKind::Sixel
    }

    fn cell_size(&self) -> CellSize {
        *self.cell.lock()
    }

    fn set_cell_size(&self, cell: CellSize) {
        *self.cell.lock() = cell;
    }

    fn render(&self, image: &RgbaImage, _id: ImageId, max: Grid, invert_colors: bool) -> Result<Frame> {
        let cell = self.cell_size();
        let fit = fit_box(image.width(), image.height(), max, cell);
        let (mut grid, mut width, mut height) = (fit.grid, fit.width, fit.height);
        // A terminal draws whole six-pixel bands and moves the cursor below
        // the last one. When that band would spill into the next row of
        // cells, trim the image to whole bands.
        if height > 6 && f64::from(height.div_ceil(6) * 6) > f64::from(grid.rows) * cell.h {
            let trimmed = height - height % 6;
            width = (u64::from(width) * u64::from(trimmed) / u64::from(height)).max(1) as u32;
            height = trimmed;
            grid = Grid::new(cells_for(width, cell.w, max.cols), cells_for(height, cell.h, max.rows));
        }
        let mut pixels = scale(image, width, height);
        if invert_colors {
            invert(&mut pixels);
        }
        Ok(Frame::Sixel { grid, data: encode(&pixels, usize::from(self.colors), self.background).into() })
    }

    fn delete(&self, _id: ImageId) -> Vec<u8> {
        Vec::new()
    }
}

/// Writes `image` as a sixel image with at most `colors` colors. Pixels
/// below half opacity stay transparent and show the cells under them. When
/// there are such pixels and a `background` is given, the image's colors take
/// one register fewer, and the background takes the register above them, at
/// least 2, selected once by an empty sixel.
fn encode(image: &RgbaImage, colors: usize, background: Option<[u8; 3]>) -> Vec<u8> {
    let pixels = image.as_raw().as_chunks::<4>().0;
    let background = background.filter(|_| pixels.iter().any(|pixel| drawn(pixel).is_none()));
    let palette = Palette::new(image, colors - usize::from(background.is_some()));
    let (w, h) = (image.width() as usize, image.height() as usize);
    let mut out = Vec::with_capacity(w * h / 4 + 64);
    out.extend_from_slice(b"\x1bP0;1;0q\"1;1;");
    push_number(&mut out, w);
    out.push(b';');
    push_number(&mut out, h);
    for (i, &color) in palette.colors.iter().enumerate() {
        push_color(&mut out, i, color);
    }
    if let Some(color) = background {
        let register = palette.colors.len().max(2);
        push_color(&mut out, register, color);
        out.push(b'#');
        push_number(&mut out, register);
        out.extend_from_slice(b"?$");
    }
    // A band is six pixel rows. Each color in it gets one line of sixels,
    // one per column, that covers only the columns where the color occurs.
    let n = palette.colors.len();
    let mut bits = vec![0u8; n * w]; // the band's pixels of each color, per column
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); n]; // the columns each color occurs in
    let mut used = Vec::new(); // the band's colors, by first column
    for y0 in (0..h).step_by(6) {
        for x in 0..w {
            for r in 0..6.min(h - y0) {
                let Some(c) = palette.index(&pixels[(y0 + r) * w + x]) else { continue };
                if bits[c * w + x] == 0 {
                    if columns[c].is_empty() {
                        used.push(c);
                    }
                    columns[c].push(x);
                }
                bits[c * w + x] |= 1 << r;
            }
        }
        for (k, &c) in used.iter().enumerate() {
            out.push(b'#');
            push_number(&mut out, c);
            let mut run = Run::default();
            let mut next = 0;
            for &x in &columns[c] {
                if x > next {
                    run.put(&mut out, b'?', x - next);
                }
                run.put(&mut out, 63 + bits[c * w + x], 1);
                bits[c * w + x] = 0;
                next = x + 1;
            }
            run.flush(&mut out);
            columns[c].clear();
            if k + 1 < used.len() {
                out.push(b'$'); // back to the band's first column for the next color
            }
        }
        used.clear();
        if y0 + 6 < h {
            out.push(b'-');
        }
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Defines color register `register` as `color`.
fn push_color(out: &mut Vec<u8>, register: usize, color: [u8; 3]) {
    out.push(b'#');
    push_number(out, register);
    out.extend_from_slice(b";2");
    for c in color {
        // Sixel color components run from 0 to 100.
        out.push(b';');
        push_number(out, (usize::from(c) * 100 + 127) / 255);
    }
}

fn push_number(out: &mut Vec<u8>, n: usize) {
    if n >= 10 {
        push_number(out, n / 10);
    }
    out.push(b'0' + (n % 10) as u8);
}

/// Repeated sixels, written as "!count char" once the run is long enough.
#[derive(Default)]
struct Run {
    sixel: u8,
    count: usize,
}

impl Run {
    fn put(&mut self, out: &mut Vec<u8>, sixel: u8, count: usize) {
        if sixel != self.sixel {
            self.flush(out);
            self.sixel = sixel;
        }
        self.count += count;
    }

    fn flush(&mut self, out: &mut Vec<u8>) {
        if self.count > 3 {
            out.push(b'!');
            push_number(out, self.count);
            out.push(self.sixel);
        } else {
            out.extend(std::iter::repeat_n(self.sixel, self.count));
        }
        *self = Run::default();
    }
}

/// The color of a pixel opaque enough to draw.
fn drawn(pixel: &[u8; 4]) -> Option<[u8; 3]> {
    (pixel[3] >= 128).then(|| [pixel[0], pixel[1], pixel[2]])
}

/// The histogram cell of a color: six bits per channel.
fn color_key([r, g, b]: [u8; 3]) -> usize {
    usize::from(r >> 2) << 12 | usize::from(g >> 2) << 6 | usize::from(b >> 2)
}

/// Maps the opaque pixels of one image to at most a given number of colors.
/// A gray image keeps its exact gray levels when they fit; others go through
/// a median cut of a histogram with six bits per channel.
struct Palette {
    colors: Vec<[u8; 3]>,
    gray: bool,
    /// Gray level or histogram key to palette index.
    lookup: Vec<u16>,
}

/// One histogram cell: its key, its color and how many pixels fell in.
#[derive(Clone, Copy)]
struct Bucket {
    key: usize,
    color: [u8; 3],
    count: usize,
}

impl Palette {
    fn new(image: &RgbaImage, colors: usize) -> Self {
        let pixels = image.as_raw().as_chunks::<4>().0;
        let mut gray = true;
        let mut grays = [0usize; 256];
        let mut counts = vec![0u32; 1 << 18];
        for color in pixels.iter().filter_map(drawn) {
            gray &= color[0] == color[1] && color[1] == color[2];
            grays[usize::from(color[0])] += 1;
            counts[color_key(color)] += 1;
        }
        let mut buckets: Vec<Bucket> = if gray {
            grays.iter().enumerate().filter(|(_, n)| **n > 0).map(|(v, &count)| Bucket { key: v, color: [v as u8; 3], count }).collect()
        } else {
            let middle = |bits: usize| ((bits & 63) as u8) << 2 | 2;
            counts
                .iter()
                .enumerate()
                .filter(|(_, n)| **n > 0)
                // The cut sorts a bucket by the middle of the colors it holds.
                .map(|(key, &n)| Bucket { key, color: [middle(key >> 12), middle(key >> 6), middle(key)], count: n as usize })
                .collect()
        };
        let boxes = median_cut(&mut buckets, colors);
        let mut lookup = vec![0u16; if gray { 256 } else { 1 << 18 }];
        for (i, range) in boxes.iter().enumerate() {
            for bucket in &buckets[range.clone()] {
                lookup[bucket.key] = i as u16;
            }
        }
        let mut palette = Palette { colors: Vec::new(), gray, lookup };
        // Each color is the mean of the pixels it stands for.
        let mut sums = vec![[0u64; 4]; boxes.len()];
        for pixel in pixels {
            if let (Some(color), Some(i)) = (drawn(pixel), palette.index(pixel)) {
                for (sum, c) in sums[i].iter_mut().zip(color) {
                    *sum += u64::from(c);
                }
                sums[i][3] += 1;
            }
        }
        palette.colors = sums.iter().map(|[r, g, b, n]| [(r / n) as u8, (g / n) as u8, (b / n) as u8]).collect();
        palette
    }

    fn index(&self, pixel: &[u8; 4]) -> Option<usize> {
        let color = drawn(pixel)?;
        Some(usize::from(self.lookup[if self.gray { usize::from(color[0]) } else { color_key(color) }]))
    }
}

/// Splits `buckets` into at most `n` groups, given as ranges of `buckets`:
/// it keeps cutting the group with the widest channel at the pixel count's
/// median along that channel.
fn median_cut(buckets: &mut [Bucket], n: usize) -> Vec<Range<usize>> {
    if buckets.is_empty() {
        return Vec::new();
    }
    let mut boxes = vec![CutBox::new(buckets, 0..buckets.len())];
    while boxes.len() < n {
        let mut widest = 0;
        for (i, cut) in boxes.iter().enumerate() {
            if cut.span > boxes[widest].span {
                widest = i;
            }
        }
        let CutBox { range, channel, span } = boxes[widest].clone();
        if span.is_none() {
            break; // every group holds one bucket
        }
        let part = &mut buckets[range.clone()];
        part.sort_by_key(|bucket| bucket.color[channel]);
        let total: usize = part.iter().map(|bucket| bucket.count).sum();
        let (mut cut, mut seen) = (1, part[0].count);
        while cut < part.len() - 1 && seen * 2 < total {
            seen += part[cut].count;
            cut += 1;
        }
        let middle = range.start + cut;
        boxes[widest] = CutBox::new(buckets, range.start..middle);
        boxes.push(CutBox::new(buckets, middle..range.end));
    }
    boxes.into_iter().map(|cut| cut.range).collect()
}

/// A group of buckets with its widest channel and that channel's span; a
/// single bucket has no span, since it cannot be cut.
#[derive(Clone)]
struct CutBox {
    range: Range<usize>,
    channel: usize,
    span: Option<u8>,
}

impl CutBox {
    fn new(buckets: &[Bucket], range: Range<usize>) -> Self {
        let mut cut = CutBox { range: range.clone(), channel: 0, span: None };
        let part = &buckets[range];
        if part.len() < 2 {
            return cut;
        }
        for channel in 0..3 {
            let values = part.iter().map(|bucket| bucket.color[channel]);
            let span = values.clone().max().unwrap_or(0) - values.min().unwrap_or(0);
            if cut.span.is_none_or(|widest| span > widest) {
                (cut.channel, cut.span) = (channel, Some(span));
            }
        }
        cut
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use yoyomi_core::image::Rgba;

    use super::*;

    /// Reads the images `encode` writes: raster size, palette and bands.
    /// Pixels no color sets stay transparent. Returns the image and the
    /// number of palette colors.
    fn decode(data: &[u8]) -> (RgbaImage, usize) {
        let text = std::str::from_utf8(data).unwrap();
        let body = text.strip_prefix("\x1bP0;1;0q\"1;1;").and_then(|s| s.strip_suffix("\x1b\\"));
        let mut s = body.unwrap_or_else(|| panic!("not a transparent 1:1 sixel image: {text:.40?}")).as_bytes();
        let number = |s: &mut &[u8]| {
            let end = s.iter().position(|c| !c.is_ascii_digit()).unwrap_or(s.len());
            let n = std::str::from_utf8(&s[..end]).unwrap().parse::<u32>().unwrap();
            *s = &s[end..];
            n
        };
        let w = number(&mut s);
        s = &s[1..];
        let h = number(&mut s);
        let mut image = RgbaImage::new(w, h);
        let mut palette = HashMap::new();
        let (mut x, mut y, mut current) = (0, 0, 0);
        while let Some(&c) = s.first() {
            match c {
                b'#' => {
                    s = &s[1..];
                    current = number(&mut s);
                    if let Some(rest) = s.strip_prefix(b";2;") {
                        s = rest;
                        let mut rgb = [0u8; 3];
                        for (k, c) in rgb.iter_mut().enumerate() {
                            if k > 0 {
                                s = &s[1..];
                            }
                            *c = ((number(&mut s) * 255 + 50) / 100) as u8;
                        }
                        palette.insert(current, Rgba([rgb[0], rgb[1], rgb[2], 255]));
                    }
                }
                b'$' => (s, x) = (&s[1..], 0),
                b'-' => (s, x, y) = (&s[1..], 0, y + 6),
                b'!' | b'?'..=b'~' => {
                    let mut count = 1;
                    let mut sixel = c;
                    if c == b'!' {
                        s = &s[1..];
                        count = number(&mut s);
                        sixel = s[0];
                    }
                    s = &s[1..];
                    for _ in 0..count {
                        for r in 0..6 {
                            if (sixel - b'?') & (1 << r) != 0 {
                                assert!(x < w && y + r < h, "sixel at {x},{} outside {w}x{h}", y + r);
                                image.put_pixel(x, y + r, palette[&current]);
                            }
                        }
                        x += 1;
                    }
                }
                other => panic!("unexpected {:?}", other as char),
            }
        }
        (image, palette.len())
    }

    /// Allows for the rounding of sixel's percent color components.
    fn near(a: Rgba<u8>, b: Rgba<u8>) -> bool {
        a[3] == b[3] && (0..3).all(|c| a[c].abs_diff(b[c]) < 3)
    }

    fn sixel(frame: Frame) -> (Grid, Vec<u8>) {
        match frame {
            Frame::Sixel { grid, data } => (grid, data.to_vec()),
            other => panic!("not a sixel frame: {other:?}"),
        }
    }

    #[test]
    fn keeps_colors_and_transparency() {
        // A partial last band and a transparent margin on the left.
        let color = |x: u32, y: u32| Rgba([(40 * (x % 4)) as u8, (60 * (y % 3)) as u8, 200, 255]);
        let image = RgbaImage::from_fn(37, 14, |x, y| if x >= 3 { color(x, y) } else { Rgba([90, 90, 90, 100]) });
        let (got, colors) = decode(&encode(&image, 256, None));
        assert_eq!((got.dimensions(), colors), ((37, 14), 12));
        for (x, y, pixel) in image.enumerate_pixels() {
            let want = if pixel[3] < 128 { Rgba([0, 0, 0, 0]) } else { *pixel };
            assert!(near(*got.get_pixel(x, y), want), "pixel {x},{y} = {:?}, want {want:?}", got.get_pixel(x, y));
        }
    }

    #[test]
    fn gray_pages_keep_every_level() {
        let image = RgbaImage::from_fn(256, 7, |x, _| Rgba([x as u8, x as u8, x as u8, 255]));
        let (got, colors) = decode(&encode(&image, 256, None));
        assert_eq!(colors, 256);
        for x in 0..256 {
            let want = Rgba([x as u8, x as u8, x as u8, 255]);
            assert!(near(*got.get_pixel(x, 6), want), "gray {x} came back as {:?}", got.get_pixel(x, 6));
        }
    }

    #[test]
    fn fits_the_terminals_colors() {
        let image = RgbaImage::from_fn(60, 20, |x, y| Rgba([(x * 4) as u8, (y * 12) as u8, (x * y) as u8, 255]));
        assert_eq!(decode(&encode(&image, 16, None)).1, 16);
        // A flat row repeats one sixel, written once with its count.
        let flat = RgbaImage::from_pixel(400, 6, Rgba([255, 255, 255, 255]));
        let data = encode(&flat, 256, None);
        assert!(data.windows(5).any(|w| w == b"!400~") && data.len() <= 60, "{:?}", String::from_utf8_lossy(&data));
    }

    /// The registers an image selects to draw with, in order, each with the
    /// offset of its `#`.
    fn selections(data: &[u8]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for at in (0..data.len()).filter(|&i| data[i] == b'#') {
            let digits = data[at + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if data[at + 1 + digits] != b';' {
                out.push((std::str::from_utf8(&data[at + 1..at + 1 + digits]).unwrap().parse().unwrap(), at));
            }
        }
        out
    }

    /// A light terminal background.
    const PAPER: [u8; 3] = [250, 250, 250];

    #[test]
    fn names_the_background_above_every_painted_register() {
        let color = |x: u32, y: u32| Rgba([(40 * (x % 4)) as u8, (60 * (y % 3)) as u8, 200, 255]);
        let image = RgbaImage::from_fn(37, 14, |x, y| if x >= 3 { color(x, y) } else { Rgba([0, 0, 0, 0]) });
        let data = encode(&image, 256, Some(PAPER));
        let selected = selections(&data);
        let (register, at) = selected[0];
        assert_eq!(register, 12, "the image's 12 colors take registers 0 to 11");
        assert!(data[at..].starts_with(b"#12?$"), "{:?}", String::from_utf8_lossy(&data[at..]));
        assert!(selected[1..].iter().all(|&(r, _)| r < register), "{selected:?}");
        assert!(data.windows(14).any(|w| w == b"#12;2;98;98;98"));
        let (got, colors) = decode(&data);
        assert_eq!(colors, 13);
        assert_eq!(got, decode(&encode(&image, 256, None)).0);
    }

    #[test]
    fn one_color_names_the_background_in_register_2() {
        let image = RgbaImage::from_fn(2, 1, |x, _| if x == 0 { Rgba([255, 0, 0, 255]) } else { Rgba([0, 0, 0, 0]) });
        assert_eq!(encode(&image, 256, None), b"\x1bP0;1;0q\"1;1;2;1#0;2;100;0;0#0@\x1b\\");
        assert_eq!(encode(&image, 256, Some(PAPER)), b"\x1bP0;1;0q\"1;1;2;1#0;2;100;0;0#2;2;98;98;98#2?$#0@\x1b\\");
    }

    #[test]
    fn opaque_images_leave_the_background_out() {
        let image = RgbaImage::from_fn(60, 20, |x, y| Rgba([(x * 4) as u8, (y * 12) as u8, (x * y) as u8, 255]));
        assert_eq!(encode(&image, 16, Some(PAPER)), encode(&image, 16, None));
        let gray = RgbaImage::from_fn(256, 7, |x, _| Rgba([x as u8, x as u8, x as u8, 255]));
        assert_eq!(encode(&gray, 256, Some(PAPER)), encode(&gray, 256, None));
    }

    #[test]
    fn background_takes_the_last_register() {
        // Every gray level above a transparent row.
        let gray = RgbaImage::from_fn(256, 7, |x, y| Rgba([x as u8, x as u8, x as u8, if y < 6 { 255 } else { 0 }]));
        let data = encode(&gray, 256, Some(PAPER));
        let selected = selections(&data);
        assert_eq!((selected[0].0, decode(&data).1), (255, 256));
        assert!(selected[1..].iter().all(|&(r, _)| r < 255), "{selected:?}");
        let color = RgbaImage::from_fn(60, 20, |x, y| Rgba([(x * 4) as u8, (y * 12) as u8, (x * y) as u8, if x > 0 { 255 } else { 0 }]));
        let data = encode(&color, 16, Some(PAPER));
        let selected = selections(&data);
        assert_eq!((selected[0].0, decode(&data).1), (15, 16));
        assert!(selected[1..].iter().all(|&(r, _)| r < 15), "{selected:?}");
    }

    #[test]
    fn render_fits_its_cells() {
        let page = RgbaImage::from_pixel(300, 500, Rgba([10, 20, 30, 255]));
        let sixel_renderer = Sixel::new(CellSize { w: 8.0, h: 17.0 }, 256);
        for (invert_colors, want) in [(false, Rgba([10, 20, 30, 255])), (true, Rgba([245, 235, 225, 255]))] {
            let (grid, data) = sixel(sixel_renderer.render(&page, ImageId(0), Grid::new(40, 12), invert_colors).unwrap());
            let (image, _) = decode(&data);
            let (w, h) = image.dimensions();
            assert!(grid.cols <= 40 && grid.rows <= 12, "{grid:?}");
            assert!(h.div_ceil(6) * 6 <= u32::from(grid.rows) * 17 && w <= u32::from(grid.cols) * 8, "{w}x{h} spills out of {grid:?}");
            assert!(near(*image.get_pixel(w / 2, h / 2), want), "invert={invert_colors}: {:?}", image.get_pixel(w / 2, h / 2));
        }
    }

    #[test]
    fn cells_match_the_bitmap() {
        struct Case {
            name: &'static str,
            size: (u32, u32),
            cell: CellSize,
            max: Grid,
            want_size: (u32, u32),
            want_grid: Grid,
        }
        for case in [
            // The last band ends inside the image's row, so nothing is trimmed.
            Case {
                name: "short and wide",
                size: (640, 11),
                cell: CellSize { w: 8.0, h: 16.0 },
                max: Grid::new(80, 10),
                want_size: (640, 11),
                want_grid: Grid::new(80, 1),
            },
            // A 9-pixel strip in 10-pixel rows would spill its second band into
            // the next row; trimmed to one band, it needs fewer columns.
            Case {
                name: "band spill",
                size: (100, 9),
                cell: CellSize { w: 10.0, h: 10.0 },
                max: Grid::new(10, 5),
                want_size: (66, 6),
                want_grid: Grid::new(7, 1),
            },
            // Four rows of 16.375 pixels hold 65.5 pixels; 66 need a fifth.
            Case {
                name: "fractional cells",
                size: (85, 66),
                cell: CellSize { w: 8.5, h: 16.375 },
                max: Grid::new(10, 8),
                want_size: (85, 66),
                want_grid: Grid::new(10, 5),
            },
        ] {
            let page = RgbaImage::from_pixel(case.size.0, case.size.1, Rgba([0, 0, 0, 255]));
            let (grid, data) = sixel(Sixel::new(case.cell, 256).render(&page, ImageId(0), case.max, false).unwrap());
            assert_eq!((decode(&data).0.dimensions(), grid), (case.want_size, case.want_grid), "{}", case.name);
        }
    }

    #[test]
    fn silent_terminals_get_256_colors() {
        assert_eq!(Sixel::new(CellSize::FALLBACK, 0).colors, 256);
        assert_eq!(Sixel::new(CellSize::FALLBACK, 16).colors, 16);
        assert_eq!(Sixel::new(CellSize::FALLBACK, 4096).colors, 256);
    }
}
