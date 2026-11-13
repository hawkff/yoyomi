use std::sync::Arc;

use crate::{Frame, Grid, ImageId, Pixels, Rgb};

/// A cell inside a pane's image box, counted from the box's top-left corner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct At {
    pub col: u16,
    pub row: u16,
}

/// One thing a pane draws in its image box.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Layer {
    /// `rows` rows of placeholder cells for kitty image `id`, `cols` cells
    /// wide, showing the image from its row `src_row` on.
    KittyRows { id: ImageId, cols: u16, src_row: u16, rows: u16, at: At },
    /// A sixel image. The app draws it after the cells flush and leaves the
    /// cells under it untouched.
    Sixel { data: Arc<[u8]>, grid: Grid, at: At },
    /// Half-block cells, row by row.
    Cells { grid: Grid, colors: Arc<[[Rgb; 2]]>, at: At },
    /// Plain text, one line per row.
    Text { lines: Arc<[String]>, at: At },
    /// RGBA pixels, one per cell, for a window that draws them itself.
    Pixels { image: Pixels, at: At },
}

impl Layer {
    /// The layer that shows all of `frame` with its top-left corner at `at`.
    pub fn from_frame(frame: &Frame, at: At) -> Layer {
        match frame {
            Frame::Kitty { id, grid, .. } => Layer::KittyRows { id: *id, cols: grid.cols, src_row: 0, rows: grid.rows, at },
            Frame::Sixel { grid, data } => Layer::Sixel { data: data.clone(), grid: *grid, at },
            Frame::Cells { grid, colors } => Layer::Cells { grid: *grid, colors: colors.clone(), at },
            Frame::Pixels { image, .. } => Layer::Pixels { image: image.clone(), at },
        }
    }
}

/// What one pane shows in a frame.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub layers: Vec<Layer>,
}

/// A kitty image to send to the terminal before the frame that shows it.
#[derive(Clone, Debug)]
pub struct Upload {
    pub id: ImageId,
    pub bytes: Arc<[u8]>,
}

impl Upload {
    /// The upload a kitty frame carries; `None` for other frames.
    pub fn from_frame(frame: &Frame) -> Option<Upload> {
        match frame {
            Frame::Kitty { id, upload, .. } => Some(Upload { id: *id, bytes: upload.clone() }),
            _ => None,
        }
    }
}
