//! The renderer of the window: frames of RGBA pixels, one per cell.

use std::sync::Arc;

use yoyomi_core::{CellSize, Frame, Grid, ImageId, Renderer, RendererKind, Result, RgbaImage, fit_box};
use yoyomi_render::{invert, scale};

/// A cell is one device pixel, so the pager and the strip size frames in
/// pixels.
const PIXEL: CellSize = CellSize { w: 1.0, h: 1.0 };

pub(crate) struct PixelRenderer;

impl Renderer for PixelRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Native
    }

    fn cell_size(&self) -> CellSize {
        PIXEL
    }

    fn set_cell_size(&self, _cell: CellSize) {}

    /// Scales `image` up or down to fit `max` pixels.
    fn render(&self, image: &RgbaImage, _id: ImageId, max: Grid, invert_colors: bool) -> Result<Frame> {
        let fit = fit_box(image.width(), image.height(), max, PIXEL);
        let mut pixels = if image.dimensions() == (fit.width, fit.height) { image.clone() } else { scale(image, fit.width, fit.height) };
        if invert_colors {
            invert(&mut pixels);
        }
        Ok(Frame::Pixels { grid: fit.grid, image: Arc::new(pixels) })
    }

    fn delete(&self, _id: ImageId) -> Vec<u8> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_fit_the_box_in_pixels() {
        let page = RgbaImage::from_pixel(100, 50, [10, 20, 30, 255].into());
        let Frame::Pixels { grid, image } = PixelRenderer.render(&page, ImageId(0), Grid::new(400, 400), true).unwrap() else { panic!("pixels") };
        assert_eq!((grid, image.dimensions()), (Grid::new(400, 200), (400, 200)), "small pages scale up");
        assert_eq!(image.get_pixel(0, 0).0, [245, 235, 225, 255]);
        let Frame::Pixels { image, .. } = PixelRenderer.render(&page, ImageId(0), Grid::new(100, 100), false).unwrap() else { panic!("pixels") };
        assert_eq!(*image, page, "a page of the box's width stays as it is");
    }
}
