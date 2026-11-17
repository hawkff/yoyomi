//! Kitty graphics with Unicode placeholders (virtual placements, `U=1`).
//! Placeholder cells are text to the terminal, so they mix with the rest of a
//! frame and pass through tmux panes. Outside a multiplexer the terminal can
//! also keep an image without showing it and place any rows of it at a pixel
//! offset inside a cell.

mod diacritics;

use std::io::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use parking_lot::Mutex;
use yoyomi_core::{CellSize, Frame, Grid, ImageId, Renderer, RendererKind, Result, RgbaImage, cells_for, fit_box};

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

/// The bytes that show `height` rows of kept image `id` from its row `top`,
/// `width` pixels wide, `x_offset` pixels right of the left edge and
/// `y_offset` pixels below the top of the cursor's cell, replacing the
/// image's placement there was. The cursor stays.
pub fn place(id: ImageId, width: u32, top: u32, height: u32, x_offset: u32, y_offset: u32) -> Vec<u8> {
    format!("\x1b_Ga=p,i={},p=1,x=0,y={top},w={width},h={height},X={x_offset},Y={y_offset},C=1,q=2\x1b\\", id.0).into_bytes()
}

/// The bytes that take away the placement `place` made of image `id`. The
/// terminal keeps the image.
pub fn unplace(id: ImageId) -> Vec<u8> {
    format!("\x1b_Ga=d,d=i,i={},p=1,q=2\x1b\\", id.0).into_bytes()
}

/// Draws images through the kitty graphics protocol.
pub struct Kitty {
    cell: Mutex<CellSize>,
    tmux: bool,
    slides: bool,
}

/// PNG bytes per upload chunk: 3072 bytes fill the 4096 base64 bytes the
/// protocol allows in one command.
const CHUNK: usize = 3072;

impl Kitty {
    /// A renderer for cells of `cell` pixels. With `tmux`, every command goes
    /// through tmux passthrough.
    pub fn new(cell: CellSize, tmux: bool) -> Self {
        Kitty { cell: Mutex::new(cell), tmux, slides: false }
    }

    /// The renderer that says, with `slides`, that the terminal places kept
    /// images by the pixel.
    pub fn with_slides(self, slides: bool) -> Self {
        Kitty { slides, ..self }
    }

    /// Appends an upload of `payload` in chunks: `first` holds the keys of
    /// the first command, and every command says whether more follow.
    fn chunked(&self, out: &mut Vec<u8>, first: &str, payload: &[u8]) {
        let chunks = payload.len().div_ceil(CHUNK);
        for (i, chunk) in payload.chunks(CHUNK).enumerate() {
            let more = u8::from(i + 1 < chunks);
            let keys = if i == 0 { format!("{first},m={more}") } else { format!("m={more}") };
            self.command(out, &keys, chunk);
        }
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
        self.chunked(&mut upload, &format!("a=T,q=2,f=100,t=d,i={},U=1,c={},r={}", id.0, grid.cols, grid.rows), &png);
        Ok(Frame::Kitty { id, grid, upload: upload.into() })
    }

    fn delete(&self, id: ImageId) -> Vec<u8> {
        let mut out = Vec::new();
        self.command(&mut out, &format!("a=d,d=I,i={},q=2", id.0), &[]);
        out
    }

    fn slides(&self) -> bool {
        self.slides
    }

    /// Sends raw pixels, compressed: the terminal stores what it gets, so
    /// three bytes a pixel for opaque images save a quarter of its memory.
    fn keep(&self, image: &RgbaImage, id: ImageId) -> Result<Frame> {
        let cell = self.cell_size();
        let opaque = image.pixels().all(|pixel| pixel[3] == 255);
        let mut zlib = ZlibEncoder::new(Vec::new(), Compression::fast());
        if opaque {
            let rgb: Vec<u8> = image.pixels().flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]).collect();
            zlib.write_all(&rgb)?;
        } else {
            zlib.write_all(image.as_raw())?;
        }
        let data = zlib.finish()?;
        let mut upload = Vec::with_capacity(data.len() / 3 * 4 + data.len() / CHUNK * 40 + 128);
        // As in `render`: an upload under an id in use replaces the image
        // and its placement whole.
        self.command(&mut upload, &format!("a=d,d=I,i={},q=2", id.0), &[]);
        let format = if opaque { 24 } else { 32 };
        let (width, height) = image.dimensions();
        self.chunked(&mut upload, &format!("a=t,q=2,f={format},o=z,t=d,i={},s={width},v={height}", id.0), &data);
        let grid = Grid::new(cells_for(width, cell.w, u16::MAX), cells_for(height, cell.h, u16::MAX));
        Ok(Frame::Kitty { id, grid, upload: upload.into() })
    }
}

#[cfg(test)]
mod tests {
    use yoyomi_core::image::{self, Rgba};

    use super::*;
    use crate::Sixel;
    use crate::png::tests::noise;

    const CELL: CellSize = CellSize { w: 8.0, h: 16.0 };

    /// Joins the base64 payloads of an upload and decodes them.
    fn payload(upload: &[u8]) -> Vec<u8> {
        let text = std::str::from_utf8(upload).unwrap();
        let mut payload = String::new();
        for command in text.split("_G").skip(1) {
            // base64 holds no ESC, so the payload ends at the first one.
            let command = &command[..command.find('\x1b').unwrap()];
            if let Some((_, data)) = command.split_once(';') {
                payload.push_str(data);
            }
        }
        STANDARD.decode(payload).unwrap()
    }

    /// Decodes the PNG of an upload.
    fn uploaded_image(upload: &[u8]) -> RgbaImage {
        image::load_from_memory(&payload(upload)).unwrap().to_rgba8()
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
    fn keep_sends_raw_pixels_without_a_placement() {
        let kitty = Kitty::new(CELL, false).with_slides(true);
        assert!(kitty.slides() && !Kitty::new(CELL, false).slides());
        let mut next = noise(5);
        // Noise over 160 x 100 pixels takes several chunks even compressed.
        let opaque = RgbaImage::from_fn(160, 100, |_, _| Rgba([next(), next(), next(), 255]));
        let mut clear = opaque.clone();
        clear.put_pixel(3, 4, Rgba([1, 2, 3, 0]));
        for (image, format, raw) in [
            (&opaque, 24, opaque.pixels().flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]).collect::<Vec<u8>>()),
            (&clear, 32, clear.as_raw().clone()),
        ] {
            let (grid, bytes) = upload(kitty.keep(image, ImageId(9)).unwrap());
            // 160 x 100 pixels reach into 20 x 7 cells of 8 x 16.
            assert_eq!(grid, Grid::new(20, 7));
            let text = String::from_utf8(bytes.clone()).unwrap();
            let first = format!("\x1b_Ga=d,d=I,i=9,q=2\x1b\\\x1b_Ga=t,q=2,f={format},o=z,t=d,i=9,s=160,v=100,m=1;");
            assert!(text.starts_with(&first), "{:?}", &text[..80]);
            assert!(!text.contains("U=1") && !text.contains("a=T") && !text.contains("a=p"));
            assert!(text.matches("\x1b_Gm=").count() >= 1 && text.ends_with("\x1b\\"));
            let mut inflated = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&payload(&bytes)[..]), &mut inflated).unwrap();
            assert!(inflated == raw, "f={format}: the payload inflates to other pixels");
        }
        assert!(Sixel::new(CELL, 256).keep(&opaque, ImageId(9)).is_err(), "other renderers keep nothing");
    }

    #[test]
    fn place_and_unplace_bytes() {
        assert_eq!(place(ImageId(4611), 900, 1720, 1300, 7, 17), b"\x1b_Ga=p,i=4611,p=1,x=0,y=1720,w=900,h=1300,X=7,Y=17,C=1,q=2\x1b\\");
        assert_eq!(unplace(ImageId(4611)), b"\x1b_Ga=d,d=i,i=4611,p=1,q=2\x1b\\");
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
