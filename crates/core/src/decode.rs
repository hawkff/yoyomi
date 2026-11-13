use std::io::{self, BufRead, Cursor, Read, Seek, SeekFrom};

use image::{ImageReader, RgbaImage};

use crate::{Cancel, Error, Result};

/// The most bytes one page may take before decoding.
pub const MAX_PAGE_BYTES: usize = 64 << 20;

/// The most pixels one decoded page may hold.
pub const MAX_PAGE_PIXELS: u64 = 32_000_000;

/// Fails unless a page of `width` x `height` pixels has pixels and stays
/// within `MAX_PAGE_PIXELS`.
pub fn check_size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(Error::Decode(format!("page has no pixels ({width}x{height})")));
    }
    if u64::from(width) * u64::from(height) > MAX_PAGE_PIXELS {
        return Err(Error::Limit(format!("page exceeds {MAX_PAGE_PIXELS} decoded pixels ({width}x{height})")));
    }
    Ok(())
}

/// Reads the pixel size of an encoded page from its header.
pub fn decoded_size(bytes: &[u8]) -> Result<(u32, u32)> {
    Ok(reader(Cursor::new(bytes))?.into_dimensions()?)
}

/// Decodes a JPEG, PNG, GIF, WebP, BMP or binary PPM page to RGBA. The
/// format comes from the data, not a file name, and the limits above hold.
/// The decoder's reads check `cancel`, so a cancelled request stops the
/// decoders that read as they go.
// shortcut: the JPEG decoder reads all its input before it decodes, so a
// JPEG stops only before and after; a streaming JPEG decoder would fix that.
pub fn decode(bytes: &[u8], cancel: &Cancel) -> Result<RgbaImage> {
    decode_from(Cursor::new(bytes), bytes, cancel)
}

fn decode_from(input: impl BufRead + Seek, bytes: &[u8], cancel: &Cancel) -> Result<RgbaImage> {
    cancel.check()?;
    if bytes.len() > MAX_PAGE_BYTES {
        return Err(Error::Limit(format!("page data exceeds {MAX_PAGE_BYTES} bytes")));
    }
    let (width, height) = decoded_size(bytes)?;
    check_size(width, height)?;
    let image = reader(Checked { input, cancel }).and_then(|reader| Ok(reader.decode()?));
    // A cancelled read fails the decoder with an I/O error.
    cancel.check()?;
    Ok(image?.into_rgba8())
}

/// Bytes read in steps of at most `CHECKED_READ`, failing once `cancel`
/// fires, as a decoder's input.
struct Checked<'a, R> {
    input: R,
    cancel: &'a Cancel,
}

const CHECKED_READ: usize = 64 << 10;

impl<R: BufRead> Read for Checked<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let src = self.fill_buf()?;
        let n = src.len().min(buf.len());
        buf[..n].copy_from_slice(&src[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Checked<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other(Error::Cancelled));
        }
        let buf = self.input.fill_buf()?;
        Ok(&buf[..buf.len().min(CHECKED_READ)])
    }

    fn consume(&mut self, amount: usize) {
        self.input.consume(amount);
    }
}

impl<R: Seek> Seek for Checked<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.input.seek(pos)
    }
}

fn reader<R: BufRead + Seek>(input: R) -> Result<ImageReader<R>> {
    let reader = ImageReader::new(input).with_guessed_format()?;
    if reader.format().is_none() {
        return Err(Error::Decode("unknown image format".into()));
    }
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255])).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn decodes_by_content() {
        let bytes = png(3, 2);
        assert_eq!(decoded_size(&bytes).unwrap(), (3, 2));
        let image = decode(&bytes, &Cancel::new()).unwrap();
        assert_eq!(image.dimensions(), (3, 2));
        assert_eq!(image.get_pixel(2, 1).0, [10, 20, 30, 255]);
    }

    #[test]
    fn binary_ppm_decodes() {
        let mut bytes = b"P6\n2 1\n255\n".to_vec();
        bytes.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
        let image = decode(&bytes, &Cancel::new()).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn rejects_garbage_and_huge_pages() {
        assert!(matches!(decode(b"not an image", &Cancel::new()), Err(Error::Decode(_))));
        assert!(matches!(check_size(8000, 4001), Err(Error::Limit(_))));
        assert!(check_size(8000, 4000).is_ok());
        assert!(matches!(check_size(0, 10), Err(Error::Decode(_))));
    }

    #[test]
    fn a_cancelled_token_stops_decoding() {
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(decode(&png(1, 1), &cancel).unwrap_err().is_cancelled());
    }

    /// Input that cancels `cancel` once a read reaches byte `at`.
    struct Tripwire<'a> {
        data: Cursor<&'a [u8]>,
        at: u64,
        cancel: &'a Cancel,
    }

    impl Read for Tripwire<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.data.read(buf)?;
            if self.data.position() >= self.at {
                self.cancel.cancel();
            }
            Ok(n)
        }
    }

    impl Seek for Tripwire<'_> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.data.seek(pos)
        }
    }

    #[test]
    fn a_cancel_during_decoding_stops_the_reads() {
        // Noise does not compress, so the PNG holds megabytes of image data.
        let mut state = 0x2545_f491_u32;
        let noise = RgbaImage::from_fn(1024, 1024, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            image::Rgba(state.to_le_bytes())
        });
        let mut bytes = Cursor::new(Vec::new());
        noise.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let bytes = bytes.into_inner();
        let cancel = Cancel::new();
        let mut input = Tripwire { data: Cursor::new(bytes.as_slice()), at: bytes.len() as u64 / 4, cancel: &cancel };
        let error = decode_from(io::BufReader::new(&mut input), &bytes, &cancel).unwrap_err();
        assert!(error.is_cancelled(), "{error}");
        assert!(input.data.position() < bytes.len() as u64 / 2, "the decoder read on after the cancel");
    }
}
