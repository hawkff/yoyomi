//! A PNG encoder tuned for terminal frames.

use std::io;

use ::png::{BitDepth, ColorType, Compression, Encoder, Filter};
use yoyomi_core::{Result, RgbaImage};

/// Appends `image` to `out` as PNG: RGB when every pixel is opaque, RGBA
/// otherwise.
pub(crate) fn encode(image: &RgbaImage, out: &mut Vec<u8>) -> Result<()> {
    let opaque = image.pixels().all(|pixel| pixel[3] == 255);
    let mut encoder = Encoder::new(out, image.width(), image.height());
    encoder.set_color(if opaque { ColorType::Rgb } else { ColorType::Rgba });
    encoder.set_depth(BitDepth::Eight);
    encoder.set_compression(Compression::Fast);
    // The Up filter on every row: trying all five per row doubles the
    // encoding time of a frame for a few percent of size.
    encoder.set_filter(Filter::Up);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    let written = if opaque {
        let rgb: Vec<u8> = image.pixels().flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]).collect();
        writer.write_image_data(&rgb)
    } else {
        writer.write_image_data(image.as_raw())
    };
    written.map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use yoyomi_core::image::{self, ColorType as DecodedColor, Rgba};

    use super::*;

    /// A small deterministic generator, so the tests need no random crate.
    pub(crate) fn noise(seed: u64) -> impl FnMut() -> u8 {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        }
    }

    #[test]
    fn encode_round_trips() {
        let mut next = noise(7);
        for (w, h) in [(1, 1), (3, 5), (97, 61)] {
            for transparent in [false, true] {
                let mut frame = RgbaImage::from_fn(w, h, |_, _| Rgba([next(), next(), next(), if transparent { next() } else { 255 }]));
                if transparent {
                    frame.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
                }
                let mut out = Vec::new();
                encode(&frame, &mut out).unwrap();
                let decoded = image::load_from_memory(&out).unwrap();
                let color = if transparent { DecodedColor::Rgba8 } else { DecodedColor::Rgb8 };
                assert_eq!(decoded.color(), color, "{w}x{h}");
                assert_eq!(decoded.to_rgba8(), frame, "{w}x{h} transparent={transparent}");
            }
        }
    }

    #[test]
    #[ignore = "a timing to compare encoders, run with --release --ignored --nocapture"]
    fn encode_time_of_a_page() {
        let mut next = noise(1);
        // Scanned pages are mostly flat paper with some ink.
        let page = RgbaImage::from_fn(1000, 1400, |x, y| {
            let ink = (x / 7 + y / 11) % 5 == 0;
            let v = if ink { next() / 4 } else { 240 + next() % 8 };
            Rgba([v, v, v, 255])
        });
        let mut out = Vec::new();
        let start = Instant::now();
        for _ in 0..20 {
            out.clear();
            encode(&page, &mut out).unwrap();
        }
        println!("1000x1400 page: {:?} per encode, {} bytes", start.elapsed() / 20, out.len());
    }
}
