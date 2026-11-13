/// Pixels per terminal cell. Sizes taken from the window size are fractional.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellSize {
    pub w: f64,
    pub h: f64,
}

impl CellSize {
    /// The size yoyomi assumes when the terminal reports none.
    pub const FALLBACK: CellSize = CellSize { w: 8.0, h: 16.0 };
}

/// A box of terminal cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
}

impl Grid {
    pub const fn new(cols: u16, rows: u16) -> Self {
        Grid { cols, rows }
    }
}

/// Where an image lands when it fits a box of cells: the cells it covers and
/// its size in pixels after scaling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    pub grid: Grid,
    pub width: u32,
    pub height: u32,
}

/// Scales an image of `width` x `height` pixels to fit `max` cells of `cell`
/// pixels each and keeps its aspect ratio. An empty image or box fits one cell
/// and one pixel.
pub fn fit_box(width: u32, height: u32, max: Grid, cell: CellSize) -> Fit {
    if width == 0 || height == 0 || max.cols == 0 || max.rows == 0 {
        return Fit { grid: Grid::new(1, 1), width: 1, height: 1 };
    }
    let box_w = f64::from(max.cols) * cell.w;
    let box_h = f64::from(max.rows) * cell.h;
    let scale = (box_w / f64::from(width)).min(box_h / f64::from(height));
    let w = ((f64::from(width) * scale) as u32).max(1);
    let h = ((f64::from(height) * scale) as u32).max(1);
    Fit { grid: Grid::new(cells_for(w, cell.w, max.cols), cells_for(h, cell.h, max.rows)), width: w, height: h }
}

/// How many cells of `cell` pixels `px` pixels reach into, from 1 to `limit`.
/// A quotient within float error of a whole number counts as that number.
pub fn cells_for(px: u32, cell: f64, limit: u16) -> u16 {
    let cells = (f64::from(px) / cell - 1e-9).ceil().max(1.0);
    cells.min(f64::from(limit)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellSize = CellSize { w: 8.0, h: 16.0 };

    #[test]
    fn an_exact_fit_fills_the_box() {
        assert_eq!(fit_box(160, 96, Grid::new(20, 6), CELL), Fit { grid: Grid::new(20, 6), width: 160, height: 96 });
    }

    #[test]
    fn tall_pages_fit_by_height() {
        // The box is 800 x 160 pixels, so the page scales by 0.08.
        assert_eq!(fit_box(1000, 2000, Grid::new(100, 10), CELL), Fit { grid: Grid::new(10, 10), width: 80, height: 160 });
    }

    #[test]
    fn wide_pages_fit_by_width() {
        // The box is 80 x 1600 pixels, so the page scales by 0.04.
        assert_eq!(fit_box(2000, 1000, Grid::new(10, 100), CELL), Fit { grid: Grid::new(10, 3), width: 80, height: 40 });
    }

    #[test]
    fn quotients_within_float_error_stay_whole() {
        assert_eq!(cells_for(30, 10.0 - 1e-12, 10), 3);
        assert_eq!(cells_for(31, 10.0, 10), 4);
    }

    #[test]
    fn cells_stay_within_one_and_the_limit() {
        assert_eq!(cells_for(0, 8.0, 5), 1);
        assert_eq!(cells_for(1000, 8.0, 5), 5);
        assert_eq!(cells_for(10, 0.0, 5), 5);
    }

    #[test]
    fn empty_inputs_fit_one_cell() {
        let one = Fit { grid: Grid::new(1, 1), width: 1, height: 1 };
        assert_eq!(fit_box(0, 10, Grid::new(5, 5), CELL), one);
        assert_eq!(fit_box(10, 10, Grid::new(0, 5), CELL), one);
    }
}
