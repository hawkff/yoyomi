//! Terminal graphics: the kitty, sixel and half-block renderers, the startup
//! probe that picks one, and the image operations they share.

mod halfblock;
mod kitty;
mod ops;
mod png;
mod sixel;
mod terminal;

pub use halfblock::HalfBlock;
pub use kitty::{Kitty, MAX_CELLS, placeholder};
pub use ops::{apply, crop_zoom, highlight, invert, rotate, scale};
pub use sixel::Sixel;
pub use terminal::{Terminal, cell_size, detect, needs_probe, probe};
