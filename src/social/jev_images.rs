//! In-memory still-image preparation for TypeSafe's experimental image input.
//! The caller supplies current-message PNG/JPEG previews; animated sources must
//! first use the shared bounded contact-sheet renderer. Originals are untouched.
use std::io::Cursor;

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits,
    Rgba, RgbaImage, codecs::png::PngEncoder, imageops,
};
use serde::Serialize;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MIN_EDGE: u32 = 280;
const MAX_EDGE: u32 = 1568;

#[derive(Serialize)]
pub struct ImageAsset {
    pub r#type: &'static str,
    pub format: &'static str,
    pub data_base64: String,
}

pub fn normalize(data_url: &str) -> Result<ImageAsset> {
    ensure!(
        data_url.len() <= MAX_BYTES.div_ceil(3) * 4 + 32,
        "jev_image_too_large"
    );
    let (prefix, encoded) = data_url
        .split_once(',')
        .ok_or_else(|| anyhow::anyhow!("jev_image_invalid"))?;
    let format = match prefix {
        "data:image/png;base64" => ImageFormat::Png,
        "data:image/jpeg;base64" => ImageFormat::Jpeg,
        _ => anyhow::bail!("jev_image_preview_required"),
    };
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| anyhow::anyhow!("jev_image_invalid"))?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_BYTES,
        "jev_image_too_large"
    );
    if format == ImageFormat::Png {
        let png = image::codecs::png::PngDecoder::new(Cursor::new(&bytes))
            .map_err(|_| anyhow::anyhow!("jev_image_invalid"))?;
        ensure!(
            !png.is_apng()
                .map_err(|_| anyhow::anyhow!("jev_image_invalid"))?,
            "jev_image_preview_required"
        );
    }
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| anyhow::anyhow!("jev_image_invalid"))?;
    let orientation = decoder
        .orientation()
        .map_err(|_| anyhow::anyhow!("jev_image_invalid"))?;
    let mut image =
        DynamicImage::from_decoder(decoder).map_err(|_| anyhow::anyhow!("jev_image_invalid"))?;
    image.apply_orientation(orientation);
    ensure!(image.width() > 0 && image.height() > 0, "jev_image_invalid");
    let thumbnail = if image.width() > MAX_EDGE || image.height() > MAX_EDGE {
        image.thumbnail(MAX_EDGE, MAX_EDGE).into_rgba8()
    } else {
        image.into_rgba8()
    };
    // Pad small or very narrow previews instead of changing their aspect ratio.
    let mut canvas = RgbaImage::from_pixel(
        thumbnail.width().max(MIN_EDGE),
        thumbnail.height().max(MIN_EDGE),
        Rgba([245, 245, 245, 255]),
    );
    let x = (canvas.width() - thumbnail.width()) / 2;
    let y = (canvas.height() - thumbnail.height()) / 2;
    imageops::overlay(&mut canvas, &thumbnail, i64::from(x), i64::from(y));
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(
            canvas.as_raw(),
            canvas.width(),
            canvas.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|_| anyhow::anyhow!("jev_image_encode_failed"))?;
    ensure!(png.len() <= MAX_BYTES, "jev_image_too_large");
    Ok(ImageAsset {
        r#type: "image",
        format: "png",
        data_base64: STANDARD.encode(png),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(width: u32, height: u32) -> String {
        let pixels = RgbaImage::from_pixel(width, height, Rgba([20, 100, 160, 255]));
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(pixels.as_raw(), width, height, ExtendedColorType::Rgba8)
            .unwrap();
        format!("data:image/png;base64,{}", STANDARD.encode(png))
    }
    #[test]
    fn pads_small_previews_and_bounds_narrow_large_images() {
        for (source, expected) in [
            ((40, 80), (280, 280)),
            ((2000, 100), (1568, 280)),
            ((512, 560), (512, 560)),
        ] {
            let asset = normalize(&input(source.0, source.1)).unwrap();
            let decoded =
                image::load_from_memory(&STANDARD.decode(asset.data_base64).unwrap()).unwrap();
            assert_eq!((decoded.width(), decoded.height()), expected);
        }
    }
    #[test]
    fn rejects_external_urls_and_unprepared_animation_without_echoing_data() {
        for input in [
            "https://example.com/photo.png?token=EXAMPLE_TOKEN",
            "file:///private/image.png",
            "data:image/gif;base64,R0lGODlh",
            "data:image/png;base64,private-marker",
        ] {
            let error = normalize(input).err().unwrap().to_string();
            assert!(!error.contains("private-marker") && !error.contains("EXAMPLE_TOKEN"));
        }
    }
}
