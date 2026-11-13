//! Half blocks: U+2580 with truecolor foreground and background, two pixels
//! per cell. They work in any 24-bit color terminal.

use parking_lot::Mutex;
use yoyomi_core::image::Rgba;
use yoyomi_core::{CellSize, Frame, Grid, ImageId, Renderer, RendererKind, Result, Rgb, RgbaImage, fit_box};

use crate::ops::{invert, scale};

/// Draws images as half-block cells.
pub struct HalfBlock {
    cell: Mutex<CellSize>,
}

impl HalfBlock {
    /// A renderer that keeps the proportions of cells of `cell` pixels.
    pub fn new(cell: CellSize) -> Self {
        HalfBlock { cell: Mutex::new(cell) }
    }
}

impl Renderer for HalfBlock {
    fn kind(&self) -> RendererKind {
        RendererKind::HalfBlock
    }

    fn cell_size(&self) -> CellSize {
        *self.cell.lock()
    }

    fn set_cell_size(&self, cell: CellSize) {
        *self.cell.lock() = cell;
    }

    fn render(&self, image: &RgbaImage, _id: ImageId, max: Grid, invert_colors: bool) -> Result<Frame> {
        // A cell holds one pixel across and two down.
        let grid = fit_box(image.width(), image.height(), max, self.cell_size()).grid;
        let mut pixels = scale(image, u32::from(grid.cols), 2 * u32::from(grid.rows));
        if invert_colors {
            invert(&mut pixels);
        }
        // Cells have no transparency, so pixels blend over black.
        let rgb = |x, y| {
            let Rgba([r, g, b, a]) = *pixels.get_pixel(x, y);
            let over_black = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
            Rgb(over_black(r), over_black(g), over_black(b))
        };
        let colors = (0..u32::from(grid.rows)).flat_map(|row| (0..u32::from(grid.cols)).map(move |col| [rgb(col, 2 * row), rgb(col, 2 * row + 1)]));
        Ok(Frame::Cells { grid, colors: colors.collect() })
    }

    fn delete(&self, id: ImageId) -> Vec<u8> {
        let _ = id;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(frame: Frame) -> (Grid, Vec<[Rgb; 2]>) {
        match frame {
            Frame::Cells { grid, colors } => (grid, colors.to_vec()),
            other => panic!("not a half-block frame: {other:?}"),
        }
    }

    #[test]
    fn cells_carry_top_and_bottom_pixels() {
        let half = HalfBlock::new(CellSize { w: 8.0, h: 16.0 });
        let white = RgbaImage::from_pixel(4, 2, Rgba([255, 255, 255, 255]));
        for (invert_colors, color) in [(false, Rgb(255, 255, 255)), (true, Rgb(0, 0, 0))] {
            let (grid, colors) = cells(half.render(&white, ImageId(0), Grid::new(4, 1), invert_colors).unwrap());
            assert_eq!((grid, colors), (Grid::new(4, 1), vec![[color, color]; 4]));
        }
        // Red over blue in one cell, and transparency shows black.
        let mut image = RgbaImage::from_pixel(2, 4, Rgba([0, 0, 255, 255]));
        image.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        image.put_pixel(1, 0, Rgba([255, 0, 0, 255]));
        image.put_pixel(1, 3, Rgba([255, 255, 255, 0]));
        image.put_pixel(0, 3, Rgba([255, 255, 255, 0]));
        let (grid, colors) = cells(half.render(&image, ImageId(0), Grid::new(2, 2), false).unwrap());
        assert_eq!(grid, Grid::new(2, 2));
        assert_eq!(colors[0], [Rgb(255, 0, 0), Rgb(0, 0, 255)]);
        assert_eq!(colors[3], [Rgb(0, 0, 255), Rgb(0, 0, 0)]);
    }

    #[test]
    fn renders_while_the_cell_size_changes() {
        let half = HalfBlock::new(CellSize { w: 8.0, h: 16.0 });
        let image = RgbaImage::new(8, 8);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..40 {
                    half.set_cell_size(CellSize { w: f64::from(8 + i % 2), h: f64::from(16 + i % 2) });
                }
            });
            for i in 0..40 {
                half.render(&image, ImageId(1), Grid::new(2, 2), i % 2 == 0).unwrap();
            }
        });
    }
}
