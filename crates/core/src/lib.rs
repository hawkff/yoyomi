//! Shared contracts of yoyomi.
//!
//! Every other crate builds on the types and traits here: books and their
//! pages, renderers and the frames they produce, the scenes a pane shows, and
//! the utilities several crates share (cancellation, decoding, the page cache,
//! the job pool and kitty image ids). Changes to this crate add items; they do
//! not rename or remove them.

mod book;
mod cache;
mod cancel;
mod decode;
mod error;
mod geometry;
mod ids;
mod jobs;
mod render;
mod scene;
mod transform;

pub use book::{Book, Chapter, Encoded, Link, LinkSpan, PageKind, Pixels, TextBox, TextLayer, TextLine, current_chapter};
pub use cache::PageCache;
pub use cancel::Cancel;
pub use decode::{MAX_PAGE_BYTES, MAX_PAGE_PIXELS, check_size, decode, decoded_size};
pub use error::{Error, Result};
pub use geometry::{CellSize, Fit, Grid, cells_for, fit_box};
pub use ids::{ImageId, ImageIds};
pub use jobs::{Jobs, Priority};
pub use render::{Frame, Renderer, RendererKind, Rgb};
pub use scene::{At, Layer, Scene, Upload};
pub use transform::{PageRect, Transform};

/// The image crate, re-exported so every crate shares one pixel type.
pub use image;
pub use image::RgbaImage;
