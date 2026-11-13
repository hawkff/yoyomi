use std::sync::Arc;

use image::RgbaImage;

use crate::{CellSize, Grid, ImageId, Pixels, Result};

/// A 24-bit color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// The terminal graphics a renderer speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RendererKind {
    /// The kitty graphics protocol with Unicode placeholders.
    Kitty,
    /// DEC sixel graphics.
    Sixel,
    /// Two pixels per cell with U+2580 and truecolor.
    HalfBlock,
    /// RGBA pixels for a window that draws them itself, one pixel per cell.
    Native,
}

impl RendererKind {
    /// The name the `-r` flag takes and the status bar shows.
    pub fn name(self) -> &'static str {
        match self {
            RendererKind::Kitty => "kitty",
            RendererKind::Sixel => "sixel",
            RendererKind::HalfBlock => "halfblock",
            RendererKind::Native => "native",
        }
    }

    /// The terminal renderer `name` stands for.
    pub fn parse(name: &str) -> Option<Self> {
        [RendererKind::Kitty, RendererKind::Sixel, RendererKind::HalfBlock].into_iter().find(|kind| kind.name() == name)
    }
}

/// One image encoded for the terminal and fitted into a box of cells.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Frame {
    /// A kitty image. The terminal stores it under `id` once `upload`
    /// arrives, and placeholder cells show it.
    Kitty { id: ImageId, grid: Grid, upload: Arc<[u8]> },
    /// A sixel image, drawn with its top-left corner on the box's first cell.
    Sixel { grid: Grid, data: Arc<[u8]> },
    /// Half-block cells, row by row: each cell's top and bottom pixel.
    Cells { grid: Grid, colors: Arc<[[Rgb; 2]]> },
    /// RGBA pixels, one per cell of `grid`.
    Pixels { grid: Grid, image: Pixels },
}

impl Frame {
    /// The cells the frame covers.
    pub fn grid(&self) -> Grid {
        match self {
            Frame::Kitty { grid, .. } | Frame::Sixel { grid, .. } | Frame::Cells { grid, .. } | Frame::Pixels { grid, .. } => *grid,
        }
    }
}

/// Converts images into terminal graphics.
pub trait Renderer: Send + Sync {
    fn kind(&self) -> RendererKind;

    /// The cell size the renderer fits images to.
    fn cell_size(&self) -> CellSize;

    /// Takes a new cell size after a resize or a font change.
    fn set_cell_size(&self, cell: CellSize);

    /// Fits `image` into at most `max` cells, keeping its aspect ratio, and
    /// encodes it. `id` names the image in the terminal; only kitty uses it.
    /// With `invert`, colors invert after scaling, where it costs less.
    fn render(&self, image: &RgbaImage, id: ImageId, max: Grid, invert: bool) -> Result<Frame>;

    /// Bytes that free image `id` in the terminal; empty for renderers that
    /// store no images there.
    fn delete(&self, id: ImageId) -> Vec<u8>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for kind in [RendererKind::Kitty, RendererKind::Sixel, RendererKind::HalfBlock] {
            assert_eq!(RendererKind::parse(kind.name()), Some(kind));
        }
        assert_eq!(RendererKind::parse("iterm"), None);
        assert_eq!(RendererKind::parse(RendererKind::Native.name()), None, "the window is no terminal renderer");
    }
}
