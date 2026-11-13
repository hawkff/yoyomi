//! Kitty graphics with Unicode placeholders (virtual placements, `U=1`).
//! Placeholder cells are text to the terminal, so they mix with the rest of a
//! frame and pass through tmux panes.

mod diacritics;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use parking_lot::Mutex;
use yoyomi_core::{CellSize, Frame, Grid, ImageId, Renderer, RendererKind, Result, RgbaImage, fit_box};

use self::diacritics::DIACRITICS;
use crate::ops::{invert, scale};

/// The most rows or columns one image can span: the size of kitty's
/// diacritic table.
pub const MAX_CELLS: u16 = DIACRITICS.len() as u16;

/// The text of one placeholder cell: U+10EEEE, then the diacritics that name
/// the cell's image row and column. The cell's foreground color names the
/// image (`ImageId::color`).
///
/// # Panics
///
/// When `row` or `col` reaches `MAX_CELLS`.
pub fn placeholder(row: u16, col: u16) -> [char; 3] {
    ['\u{10EEEE}', DIACRITICS[usize::from(row)], DIACRITICS[usize::from(col)]]
}

/// Draws images through the kitty graphics protocol.
pub struct Kitty {
    cell: Mutex<CellSize>,
    tmux: bool,
}

/// PNG bytes per upload chunk: 3072 bytes fill the 4096 base64 bytes the
/// protocol allows in one command.
const CHUNK: usize = 3072;

impl Kitty {
    /// A renderer for cells of `cell` pixels. With `tmux`, every command goes
    /// through tmux passthrough.
    pub fn new(cell: CellSize, tmux: bool) -> Self {
        Kitty { cell: Mutex::new(cell), tmux }
    }

    /// Appends one graphics command with a base64 payload. tmux passthrough
    /// wants every ESC inside doubled; base64 holds none, so only the framing
    /// changes.
    fn command(&self, out: &mut Vec<u8>, keys: &str, payload: &[u8]) {
        if self.tmux {
            out.extend_from_slice(b"\x1bPtmux;\x1b");
        }
        out.extend_from_slice(b"\x1b_G");
        out.extend_from_slice(keys.as_bytes());
        if !payload.is_empty() {
            out.push(b';');
            let start = out.len();
            out.resize(start + payload.len().div_ceil(3) * 4, 0);
            let written = STANDARD.encode_slice(payload, &mut out[start..]).expect("the buffer fits the base64 text");
            out.truncate(start + written);
        }
        out.extend_from_slice(if self.tmux { b"\x1b\x1b\\\x1b\\".as_slice() } else { b"\x1b\\" });
    }
}

impl Renderer for Kitty {
    fn kind(&self) -> RendererKind {
        RendererKind::Kitty
    }

    fn cell_size(&self) -> CellSize {
        *self.cell.lock()
    }

    fn set_cell_size(&self, cell: CellSize) {
        *self.cell.lock() = cell;
    }

    fn render(&self, image: &RgbaImage, id: ImageId, max: Grid, invert_colors: bool) -> Result<Frame> {
        let cell = self.cell_size();
        let max = Grid::new(max.cols.min(MAX_CELLS), max.rows.min(MAX_CELLS));
        let grid = fit_box(image.width(), image.height(), max, cell).grid;
        // The terminal stretches the bitmap over the whole grid from its top
        // left corner, so the bitmap fills the grid exactly and leaves no
        // slack that would show as an off-center page.
        let width = ((f64::from(grid.cols) * cell.w).round() as u32).max(1);
        let height = ((f64::from(grid.rows) * cell.h).round() as u32).max(1);
        let mut changed = (image.dimensions() != (width, height)).then(|| scale(image, width, height));
        if invert_colors {
            invert(changed.get_or_insert_with(|| image.clone()));
        }
        let mut png = Vec::new();
        crate::png::encode(changed.as_ref().unwrap_or(image), &mut png)?;

        let mut upload = Vec::with_capacity(png.len() / 3 * 4 + png.len() / CHUNK * 40 + 128);
        // Deleting first drops the old placement too. Reusing an id with
        // another grid otherwise keeps the old placement's geometry, and the
        // page shows half drawn.
        self.command(&mut upload, &format!("a=d,d=I,i={},q=2", id.0), &[]);
        let chunks = png.len().div_ceil(CHUNK);
        for (i, chunk) in png.chunks(CHUNK).enumerate() {
            let more = u8::from(i + 1 < chunks);
            let keys =
                if i == 0 { format!("a=T,q=2,f=100,t=d,i={},U=1,c={},r={},m={more}", id.0, grid.cols, grid.rows) } else { format!("m={more}") };
            self.command(&mut upload, &keys, chunk);
        }
        Ok(Frame::Kitty { id, grid, upload: upload.into() })
    }

    fn delete(&self, id: ImageId) -> Vec<u8> {
        let mut out = Vec::new();
        self.command(&mut out, &format!("a=d,d=I,i={},q=2", id.0), &[]);
        out
    }
}

#[cfg(test)]
mod tests {
    use yoyomi_core::image::{self, Rgba};

    use super::*;
    use crate::png::tests::noise;

    const CELL: CellSize = CellSize { w: 8.0, h: 16.0 };

    /// Joins the base64 payloads of an upload and decodes the PNG.
    fn uploaded_image(upload: &[u8]) -> RgbaImage {
        let text = std::str::from_utf8(upload).unwrap();
        let mut payload = String::new();
        for command in text.split("_G").skip(1) {
            // base64 holds no ESC, so the payload ends at the first one.
            let command = &command[..command.find('\x1b').unwrap()];
            if let Some((_, data)) = command.split_once(';') {
                payload.push_str(data);
            }
        }
        image::load_from_memory(&STANDARD.decode(payload).unwrap()).unwrap().to_rgba8()
    }

    fn upload(frame: Frame) -> (Grid, Vec<u8>) {
        match frame {
            Frame::Kitty { grid, upload, .. } => (grid, upload.to_vec()),
            other => panic!("not a kitty frame: {other:?}"),
        }
    }

    #[test]
    fn placeholders_name_row_and_column() {
        assert_eq!(MAX_CELLS, 297);
        assert_eq!(placeholder(0, 2), ['\u{10EEEE}', '\u{0305}', '\u{030E}']);
        assert_eq!(placeholder(296, 0)[1], '\u{1D244}');
    }

    #[test]
    fn uploads_chunk_scaled_and_exact_frames() {
        // Opaque noise spreads the PNG over several chunks; a flat block keeps
        // the probed pixel free of interpolation.
        let mut next = noise(1);
        let mut frame = RgbaImage::from_fn(160, 96, |_, _| Rgba([next(), next(), next(), 255]));
        let probe = Rgba([200, 120, 40, 255]);
        for y in 2..9 {
            for x in 2..9 {
                frame.put_pixel(x, y, probe);
            }
        }
        let before = frame.clone();
        let half = scale(&frame, 80, 48);
        for tmux in [false, true] {
            let kitty = Kitty::new(CELL, tmux);
            for invert_colors in [false, true] {
                // 160 x 96 pixels fill 20 x 6 cells of 8 x 16, the exact case.
                let (grid, bytes) = upload(kitty.render(&frame, ImageId(1), Grid::new(20, 6), invert_colors).unwrap());
                assert_eq!(grid, Grid::new(20, 6));
                assert!(bytes.windows(3).filter(|w| w == b"m=1").count() >= 2, "the upload fit one chunk");
                let mut want = before.clone();
                if invert_colors {
                    invert(&mut want);
                }
                assert_eq!(uploaded_image(&bytes), want, "tmux={tmux} invert={invert_colors}");
                // The same page in 10 x 3 cells scales to 80 x 48 pixels.
                let (grid, bytes) = upload(kitty.render(&frame, ImageId(1), Grid::new(10, 3), invert_colors).unwrap());
                let mut want = half.clone();
                if invert_colors {
                    invert(&mut want);
                }
                assert_eq!((grid, uploaded_image(&bytes)), (Grid::new(10, 3), want));
            }
        }
        assert_eq!(frame, before, "rendering changed the caller's frame");
    }

    #[test]
    fn upload_framing_matches_the_protocol() {
        let kitty = Kitty::new(CELL, false);
        let (grid, bytes) = upload(kitty.render(&RgbaImage::new(8, 8), ImageId(4611), Grid::new(2, 2), false).unwrap());
        // 8 x 8 pixels grow to 16 x 16, two columns and one row of 8 x 16.
        assert_eq!(grid, Grid::new(2, 1));
        let text = String::from_utf8(bytes).unwrap();
        let start = "\x1b_Ga=d,d=I,i=4611,q=2\x1b\\\x1b_Ga=T,q=2,f=100,t=d,i=4611,U=1,c=2,r=1,m=0;";
        assert!(text.starts_with(start) && text.ends_with("\x1b\\"), "{text:?}");
        assert_eq!(text.matches("\x1b_G").count(), 2);
        assert_eq!(kitty.delete(ImageId(4611)), b"\x1b_Ga=d,d=I,i=4611,q=2\x1b\\");

        let mut next = noise(3);
        let noisy = RgbaImage::from_fn(160, 96, |_, _| Rgba([next(), next(), next(), 255]));
        let (_, bytes) = upload(kitty.render(&noisy, ImageId(9), Grid::new(20, 6), false).unwrap());
        let text = String::from_utf8(bytes).unwrap();
        let commands: Vec<&str> = text.split("\x1b\\").filter(|c| !c.is_empty()).collect();
        let (last, middle) = commands[2..].split_last().unwrap();
        assert!(commands[1].starts_with("\x1b_Ga=T,q=2,f=100,t=d,i=9,U=1,c=20,r=6,m=1;"));
        assert_eq!(commands[1].split_once(';').unwrap().1.len(), 4096);
        for command in middle {
            assert!(command.starts_with("\x1b_Gm=1;") && command.len() == "\x1b_Gm=1;".len() + 4096, "{command:.20}");
        }
        assert!(last.starts_with("\x1b_Gm=0;"));
    }

    #[test]
    fn tmux_wraps_every_command() {
        let kitty = Kitty::new(CELL, true);
        assert_eq!(kitty.delete(ImageId(7)), b"\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=7,q=2\x1b\x1b\\\x1b\\");
        let (_, bytes) = upload(kitty.render(&RgbaImage::new(8, 8), ImageId(7), Grid::new(2, 2), false).unwrap());
        let text = String::from_utf8(bytes).unwrap();
        let commands: Vec<&str> = text.split_terminator("\x1b\x1b\\\x1b\\").collect();
        assert_eq!(commands.len(), 2, "{text:?}");
        assert!(commands.iter().all(|c| c.starts_with("\x1bPtmux;\x1b\x1b_G")));
        assert!(text.ends_with("\x1b\x1b\\\x1b\\"));
    }

    #[test]
    fn grids_stay_within_the_diacritics() {
        let tall = RgbaImage::new(1, 4000);
        let (grid, _) = upload(Kitty::new(CellSize { w: 1.0, h: 1.0 }, false).render(&tall, ImageId(1), Grid::new(400, 400), false).unwrap());
        assert_eq!(grid.rows, MAX_CELLS);
    }

    #[test]
    fn renders_while_the_cell_size_changes() {
        let kitty = Kitty::new(CELL, false);
        let image = RgbaImage::new(8, 8);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..40 {
                    kitty.set_cell_size(CellSize { w: f64::from(8 + i % 2), h: f64::from(16 + i % 2) });
                }
            });
            for i in 0..40 {
                kitty.render(&image, ImageId(1), Grid::new(2, 2), i % 2 == 0).unwrap();
            }
        });
    }
}
