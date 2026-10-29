//! Bounded preparation of native-generated PNGs for QQ's existing image path.
//!
//! This is not an input-image decoder or a file picker. Small native PNGs keep
//! their original bytes; only oversized native results are decoded and reduced.

use std::io::Cursor;

use anyhow::{Result, bail, ensure};
use image::{
    DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat, ImageReader, Limits,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops::FilterType,
};

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const MAX_NATIVE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DELIVERY_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;

/// Return one PNG/JPEG no larger than 2 MiB, without filesystem or network I/O.
/// The magic-only fast path intentionally matches existing small native-image
/// fixtures; the trusted native worker owns validation of the original result.
pub fn prepare_native_png(bytes: &[u8]) -> Result<(&'static str, Vec<u8>)> {
    ensure!(bytes.starts_with(PNG_MAGIC), "native_png_invalid");
    ensure!(bytes.len() <= MAX_NATIVE_BYTES, "native_png_too_large");
    if bytes.len() <= MAX_DELIVERY_BYTES {
        return Ok(("image/png", bytes.to_vec()));
    }

    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| anyhow::anyhow!("native_png_decode_failed"))?;

    if has_transparency(&image) {
        // Normalize to eight-bit RGBA after shrinking, preserving alpha rather
        // than flattening against an arbitrary background. 640² RGBA is under
        // 1.64 MB before PNG compression; still check the encoded result.
        let image = DynamicImage::ImageRgba8(shrink(image, 1024).into_rgba8());
        let encoded = encode_png(&image)?;
        if encoded.len() <= MAX_DELIVERY_BYTES {
            return Ok(("image/png", encoded));
        }
        let encoded = encode_png(&shrink(image, 640))?;
        ensure!(
            encoded.len() <= MAX_DELIVERY_BYTES,
            "native_image_delivery_too_large"
        );
        Ok(("image/png", encoded))
    } else {
        let image = DynamicImage::ImageRgb8(shrink(image, 2048).into_rgb8());
        let encoded = encode_jpeg(&image, 85)?;
        if encoded.len() <= MAX_DELIVERY_BYTES {
            return Ok(("image/jpeg", encoded));
        }
        let encoded = encode_jpeg(&shrink(image, 1536), 80)?;
        ensure!(
            encoded.len() <= MAX_DELIVERY_BYTES,
            "native_image_delivery_too_large"
        );
        Ok(("image/jpeg", encoded))
    }
}

/// An alpha channel alone does not imply transparency. Check its native depth
/// before conversion so an almost-opaque sixteen-bit value is not mistaken for
/// a fully opaque source and flattened into JPEG.
fn has_transparency(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageLumaA8(buffer) => buffer.pixels().any(|pixel| pixel[1] != u8::MAX),
        DynamicImage::ImageRgba8(buffer) => buffer.pixels().any(|pixel| pixel[3] != u8::MAX),
        DynamicImage::ImageLumaA16(buffer) => buffer.pixels().any(|pixel| pixel[1] != u16::MAX),
        DynamicImage::ImageRgba16(buffer) => buffer.pixels().any(|pixel| pixel[3] != u16::MAX),
        DynamicImage::ImageRgba32F(buffer) => buffer.pixels().any(|pixel| pixel[3] != 1.0),
        _ => false,
    }
}

fn shrink(image: DynamicImage, max_edge: u32) -> DynamicImage {
    if image.width() > max_edge || image.height() > max_edge {
        image.resize(max_edge, max_edge, FilterType::Triangle)
    } else {
        image
    }
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>> {
    let Some(rgba) = image.as_rgba8() else {
        bail!("native_image_encode_failed");
    };
    let mut encoded = Vec::new();
    PngEncoder::new(&mut encoded)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|_| anyhow::anyhow!("native_image_encode_failed"))?;
    Ok(encoded)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, quality)
        .encode_image(image)
        .map_err(|_| anyhow::anyhow!("native_image_encode_failed"))?;
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn png(image: &image::RgbaImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )
            .unwrap();
        bytes
    }

    /// Fixed pseudorandom bytes make incompressible, reproducible test images
    /// without randomness, checked-in binary fixtures, files or host commands.
    fn noise(width: u32, height: u32, transparent: bool) -> image::RgbaImage {
        let mut value = 0x91e1_0da5_u32;
        ImageBuffer::from_fn(width, height, |_, _| {
            let mut pixel = [0; 4];
            for channel in &mut pixel {
                value ^= value << 13;
                value ^= value >> 17;
                value ^= value << 5;
                *channel = value as u8;
            }
            if !transparent {
                pixel[3] = 255;
            }
            Rgba(pixel)
        })
    }

    #[test]
    fn small_pngs_keep_every_byte_including_the_legacy_magic_only_fixture() {
        let bytes = png(&ImageBuffer::from_pixel(4, 3, Rgba([20, 90, 120, 71])));
        for source in [&bytes[..], &b"\x89PNG\r\n\x1a\nlegacy-native-fixture"[..]] {
            let (mime, prepared) = prepare_native_png(source).unwrap();
            assert_eq!(mime, "image/png");
            assert_eq!(prepared, source);
        }
        let mut boundary = PNG_MAGIC.to_vec();
        boundary.resize(MAX_DELIVERY_BYTES, 0);
        assert_eq!(prepare_native_png(&boundary).unwrap().1, boundary);
    }

    #[test]
    fn a_large_opaque_noise_png_becomes_a_decodable_bounded_jpeg() {
        // A fully opaque RGBA channel must not force the transparent PNG path.
        let source = png(&noise(2304, 768, false));
        assert!(source.len() > MAX_DELIVERY_BYTES && source.len() <= MAX_NATIVE_BYTES);
        let (mime, prepared) = prepare_native_png(&source).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert!(prepared.len() <= MAX_DELIVERY_BYTES);
        let decoded = image::load_from_memory_with_format(&prepared, ImageFormat::Jpeg).unwrap();
        assert!(decoded.width() <= 2048 && decoded.height() <= 2048);
        assert!(decoded.width() >= 1536);
        assert!((decoded.width() as i64 - decoded.height() as i64 * 3).abs() <= 2);
        assert!(!decoded.color().has_alpha());
    }

    #[test]
    fn large_transparent_noise_keeps_alpha_and_uses_the_second_size_when_needed() {
        let source = png(&noise(1024, 768, true));
        assert!(source.len() > MAX_DELIVERY_BYTES && source.len() <= MAX_NATIVE_BYTES);
        let (mime, prepared) = prepare_native_png(&source).unwrap();
        assert_eq!(mime, "image/png");
        assert!(prepared.len() <= MAX_DELIVERY_BYTES);
        let decoded = image::load_from_memory_with_format(&prepared, ImageFormat::Png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (640, 480));
        assert!(decoded.color().has_alpha());
        assert!(decoded.to_rgba8().pixels().any(|pixel| pixel[3] < 255));
    }

    #[test]
    fn unsupported_corrupt_and_oversized_inputs_have_fixed_errors() {
        for bytes in [&b""[..], &b"not-png"[..], &b"\xff\xd8\xffjpeg"[..]] {
            assert_eq!(
                prepare_native_png(bytes).unwrap_err().to_string(),
                "native_png_invalid"
            );
        }
        let mut corrupt = PNG_MAGIC.to_vec();
        corrupt.resize(MAX_DELIVERY_BYTES + 1, 0);
        assert_eq!(
            prepare_native_png(&corrupt).unwrap_err().to_string(),
            "native_png_decode_failed"
        );
        corrupt.resize(MAX_NATIVE_BYTES + 1, 0);
        assert_eq!(
            prepare_native_png(&corrupt).unwrap_err().to_string(),
            "native_png_too_large"
        );
    }

    #[test]
    fn an_oversized_valid_png_dimension_is_rejected_before_pixel_decoding() {
        let mut source = png(&ImageBuffer::from_pixel(4097, 1, Rgba([30, 40, 50, 255])));
        // PNG permits data after IEND. Padding selects the large-image branch
        // without allocating the huge image that a malicious header might name.
        source.resize(MAX_DELIVERY_BYTES + 1, 0);
        let dimensions = ImageReader::with_format(Cursor::new(&source), ImageFormat::Png)
            .into_dimensions()
            .unwrap();
        assert_eq!(dimensions, (4097, 1));
        assert_eq!(
            prepare_native_png(&source).unwrap_err().to_string(),
            "native_png_decode_failed"
        );
    }

    #[test]
    fn shrinking_never_enlarges_small_dimensions_or_drops_transparency() {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(37, 13, Rgba([1, 2, 3, 0])));
        let image = shrink(image, 640);
        assert_eq!((image.width(), image.height()), (37, 13));
        assert!(has_transparency(&image));
        let opaque =
            DynamicImage::ImageLumaA16(ImageBuffer::from_pixel(1, 1, image::LumaA([5, u16::MAX])));
        let partial = DynamicImage::ImageLumaA16(ImageBuffer::from_pixel(
            1,
            1,
            image::LumaA([5, u16::MAX - 1]),
        ));
        assert!(!has_transparency(&opaque));
        assert!(has_transparency(&partial));
    }
}
