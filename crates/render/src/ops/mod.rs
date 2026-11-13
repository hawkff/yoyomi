//! Image operations the renderers, the view and the app share.

use std::sync::Arc;

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use yoyomi_core::image::imageops;
use yoyomi_core::{PageRect, Pixels, RgbaImage, Transform};

/// Scales `image` to exactly `width` x `height` pixels with a bilinear
/// filter.
pub fn scale(image: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let mut out = RgbaImage::new(width.max(1), height.max(1));
    if image.width() == 0 || image.height() == 0 {
        return out;
    }
    let (width, height) = out.dimensions();
    let src = ImageRef::new(image.width(), image.height(), image.as_raw(), PixelType::U8x4).expect("an RgbaImage holds 4 bytes per pixel");
    let mut dst = Image::from_slice_u8(width, height, &mut out, PixelType::U8x4).expect("an RgbaImage holds 4 bytes per pixel");
    // The resizer weighs colors by alpha, so transparent pixels lend no color
    // to the opaque ones next to them.
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    Resizer::new().resize(&src, &mut dst, &options).expect("both images hold RGBA pixels");
    out
}

/// Turns `image` clockwise by `quarter_turns` quarter turns.
pub fn rotate(image: &RgbaImage, quarter_turns: u8) -> RgbaImage {
    match quarter_turns & 3 {
        1 => imageops::rotate90(image),
        2 => imageops::rotate180(image),
        3 => imageops::rotate270(image),
        _ => image.clone(),
    }
}

/// The part of `image` a zoom shows: `1 / zoom` of each side around
/// `center`, given as fractions of the image and moved to stay inside it.
/// The crop keeps at least one pixel.
pub fn crop_zoom(image: &RgbaImage, zoom: f64, center: (f64, f64)) -> RgbaImage {
    let (w, h) = (f64::from(image.width()), f64::from(image.height()));
    if w == 0.0 || h == 0.0 {
        return image.clone();
    }
    // max and min, unlike clamp, turn a NaN into the bound.
    let (vw, vh) = ((w / zoom).max(1.0).min(w), (h / zoom).max(1.0).min(h));
    let x0 = (center.0 * w - vw / 2.0).max(0.0).min(w - vw);
    let y0 = (center.1 * h - vh / 2.0).max(0.0).min(h - vh);
    let (x, y) = (x0 as u32, y0 as u32);
    imageops::crop_imm(image, x, y, (x0 + vw) as u32 - x, (y0 + vh) as u32 - y).to_image()
}

/// Inverts the colors of `image` in place and keeps its alpha.
pub fn invert(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        for c in &mut pixel.0[..3] {
            *c = 255 - *c;
        }
    }
}

/// Tints `rects`, given as fractions of `image`, halfway toward light blue, so
/// text under a selection stays legible on plain and inverted pages.
pub fn highlight(image: &mut RgbaImage, rects: &[PageRect]) {
    const TINT: [u16; 3] = [128, 176, 255];
    let (w, h) = image.dimensions();
    // Pixels the rectangle touches at all count as inside it.
    let span = |a: f64, b: f64, size: u32| {
        let size = f64::from(size);
        ((a.min(b) * size).floor().clamp(0.0, size) as u32, (a.max(b) * size).ceil().clamp(0.0, size) as u32)
    };
    for rect in rects {
        let ((x0, x1), (y0, y1)) = (span(rect.x0, rect.x1, w), span(rect.y0, rect.y1, h));
        for y in y0..y1 {
            for x in x0..x1 {
                let pixel = image.get_pixel_mut(x, y);
                for (c, t) in pixel.0.iter_mut().zip(TINT) {
                    *c = ((u16::from(*c) + t) / 2) as u8;
                }
            }
        }
    }
}

/// Applies a transform's highlights, quarter turns and zoom crop to `page`.
/// Returns the image to render and whether the renderer still has to invert
/// it, which costs less after scaling. With highlights, the page inverts
/// first, so the tint lands on the inverted colors.
pub fn apply(page: &Pixels, transform: &Transform) -> (Pixels, bool) {
    let highlights = !transform.highlights.is_empty();
    let mut changed: Option<RgbaImage> = None;
    if highlights {
        let mut image = RgbaImage::clone(page);
        if transform.invert {
            invert(&mut image);
        }
        highlight(&mut image, &transform.highlights);
        changed = Some(image);
    }
    if transform.quarter_turns & 3 != 0 {
        changed = Some(rotate(changed.as_ref().unwrap_or(page), transform.quarter_turns));
    }
    if !transform.is_whole() {
        changed = Some(crop_zoom(changed.as_ref().unwrap_or(page), transform.zoom, transform.center));
    }
    (changed.map_or_else(|| Arc::clone(page), Arc::new), transform.invert && !highlights)
}

#[cfg(test)]
mod tests {
    use yoyomi_core::image::Rgba;

    use super::*;

    #[test]
    fn scale_hits_the_size_and_keeps_transparent_edges_colored() {
        // White beside a transparent black pixel: the blend stays white and
        // only its alpha drops.
        let image = RgbaImage::from_raw(2, 1, vec![255, 255, 255, 255, 0, 0, 0, 0]).unwrap();
        let one = scale(&image, 1, 1);
        let Rgba([r, g, b, a]) = *one.get_pixel(0, 0);
        assert_eq!([r, g, b], [255, 255, 255]);
        assert!((100..156).contains(&a), "alpha {a}");
        assert_eq!(scale(&image, 7, 3).dimensions(), (7, 3));
        assert_eq!(scale(&RgbaImage::new(0, 0), 0, 4).dimensions(), (1, 4));
    }

    #[test]
    fn invert_keeps_alpha_and_restores_colors() {
        let image = RgbaImage::from_raw(4, 1, vec![255, 255, 255, 255, 0, 0, 0, 255, 20, 40, 80, 128, 0, 0, 0, 0]).unwrap();
        let mut inverted = image.clone();
        invert(&mut inverted);
        assert_eq!(inverted.as_raw(), &vec![0, 0, 0, 255, 255, 255, 255, 255, 235, 215, 175, 128, 255, 255, 255, 0]);
        invert(&mut inverted);
        assert_eq!(inverted, image);
    }

    #[test]
    fn rotate_turns_quarter_turns_clockwise() {
        // 2 wide and 3 tall; each pixel holds its own coordinates.
        let src = RgbaImage::from_fn(2, 3, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        // Where each turn moves source pixel (x, y).
        type Lands = fn(u32, u32) -> (u32, u32);
        let cases: [(u8, (u32, u32), Lands); 3] =
            [(1, (3, 2), |x, y| (2 - y, x)), (2, (2, 3), |x, y| (1 - x, 2 - y)), (3, (3, 2), |x, y| (y, 1 - x))];
        for (turns, size, lands) in cases {
            let got = rotate(&src, turns);
            assert_eq!(got.dimensions(), size, "{turns} turns");
            for (x, y, pixel) in src.enumerate_pixels() {
                let (dx, dy) = lands(x, y);
                assert_eq!(got.get_pixel(dx, dy), pixel, "{turns} turns, pixel {x},{y}");
            }
        }
        assert_eq!(rotate(&rotate(&src, 3), 5), src);
    }

    #[test]
    fn zoom_keeps_at_least_one_pixel() {
        for (w, h) in [(1, 1), (1, 40), (40, 1), (3, 5)] {
            let src = RgbaImage::new(w, h);
            for turns in 0..4 {
                let turned = rotate(&src, turns);
                for center in [0.0, 0.5, 1.0, -3.0, f64::NAN] {
                    let got = crop_zoom(&turned, 8.0, (center, center));
                    assert!(got.width() >= 1 && got.height() >= 1, "{w}x{h}, {turns} turns, center {center}");
                    assert!(got.width() <= turned.width() && got.height() <= turned.height());
                }
            }
        }
    }

    #[test]
    fn zoom_crops_around_the_center_and_stays_inside() {
        let src = RgbaImage::from_fn(100, 40, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        let got = crop_zoom(&src, 4.0, (0.5, 0.5));
        assert_eq!((got.dimensions(), *got.get_pixel(0, 0)), ((25, 10), Rgba([37, 15, 0, 255])));
        let corner = crop_zoom(&src, 2.0, (1.0, 1.0));
        assert_eq!((corner.dimensions(), *corner.get_pixel(0, 0)), ((50, 20), Rgba([50, 20, 0, 255])));
        assert_eq!(crop_zoom(&src, 0.5, (0.2, 0.2)), src);
    }

    #[test]
    fn zoom_keeps_empty_images_empty() {
        for (w, h) in [(0, 4), (4, 0), (0, 0)] {
            assert_eq!(crop_zoom(&RgbaImage::new(w, h), 8.0, (0.5, 0.5)).dimensions(), (w, h));
        }
    }

    #[test]
    fn highlight_tints_rectangles_only() {
        let mut image = RgbaImage::from_pixel(4, 4, Rgba([255, 255, 255, 255]));
        image.put_pixel(0, 0, Rgba([0, 0, 0, 255]));
        let rects = [PageRect { x0: 0.0, y0: 0.0, x1: 0.5, y1: 0.25 }, PageRect { x0: 0.75, y0: 0.75, x1: 2.25, y1: 2.25 }];
        highlight(&mut image, &rects);
        assert_eq!(*image.get_pixel(0, 0), Rgba([64, 88, 127, 255]));
        assert_eq!(*image.get_pixel(1, 0), Rgba([191, 215, 255, 255]));
        assert_eq!(*image.get_pixel(2, 0), Rgba([255, 255, 255, 255]));
        assert_eq!(*image.get_pixel(0, 1), Rgba([255, 255, 255, 255]));
        assert_eq!(*image.get_pixel(3, 3), Rgba([191, 215, 255, 255]));
    }

    #[test]
    fn apply_without_changes_returns_the_same_page() {
        let page: Pixels = Arc::new(RgbaImage::new(3, 2));
        for invert in [false, true] {
            let (got, renderer_inverts) = apply(&page, &Transform { invert, ..Transform::default() });
            assert!(Arc::ptr_eq(&got, &page));
            assert_eq!(renderer_inverts, invert);
        }
    }

    #[test]
    fn apply_inverts_before_highlights_then_turns_and_crops() {
        // A black page with a white top-left pixel, 4 wide and 2 tall.
        let mut image = RgbaImage::from_pixel(4, 2, Rgba([0, 0, 0, 255]));
        image.put_pixel(0, 0, Rgba([255, 255, 255, 255]));
        let page: Pixels = Arc::new(image);
        let transform = Transform {
            quarter_turns: 1,
            zoom: 2.0,
            center: (1.0, 0.0),
            invert: true,
            highlights: vec![PageRect { x0: 0.0, y0: 0.0, x1: 0.25, y1: 0.5 }].into(),
        };
        let (got, renderer_inverts) = apply(&page, &transform);
        assert!(!renderer_inverts);
        // Turned, the page is 2 wide and 4 tall and the old top-left pixel
        // sits top right; the zoom keeps the top-right 1 x 2 pixels.
        assert_eq!(got.dimensions(), (1, 2));
        assert_eq!(*got.get_pixel(0, 0), Rgba([64, 88, 127, 255]));
        assert_eq!(*got.get_pixel(0, 1), Rgba([255, 255, 255, 255]));
        assert_eq!(page.get_pixel(0, 0), &Rgba([255, 255, 255, 255]), "the page itself stays as it was");
    }
}
