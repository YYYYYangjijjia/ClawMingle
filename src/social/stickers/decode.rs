//! Decode bounded visual previews without replacing the immutable original.
use anyhow::{Result, ensure};
use image::{
    AnimationDecoder, DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, Limits, Rgba, RgbaImage,
    codecs::{gif::GifDecoder, png::PngEncoder, webp::WebPDecoder},
    imageops,
};
use serde::{Deserialize, Serialize};
use std::{
    io::Cursor,
    time::{Duration, Instant},
};

pub const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_FRAMES: usize = 120;
const MAX_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_ALLOC: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrameSample {
    pub frame: usize,
    pub time_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VisualInfo {
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
    #[serde(rename = "frames")]
    pub frame_count: usize,
    pub duration_ms: u64,
    pub samples: Vec<FrameSample>,
    pub interpretation: String,
}
pub struct Decoded {
    pub info: VisualInfo,
    pub preview: Vec<u8>,
}

fn limits(edge: u32) -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(edge);
    limits.max_image_height = Some(edge);
    limits.max_alloc = Some(MAX_ALLOC);
    limits
}
fn png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    )?;
    ensure!(bytes.len() <= MAX_BYTES, "sticker_preview_too_large");
    Ok(bytes)
}
fn frames(frames: image::Frames<'_>, width: u32, height: u32, mime: &str) -> Result<Decoded> {
    ensure!(
        width > 0 && height > 0 && width <= 1024 && height <= 1024,
        "sticker_animation_dimensions_exceeded"
    );
    let started = Instant::now();
    let mut thumbnails = Vec::new();
    let mut times = Vec::new();
    let mut duration = 0u64;
    for frame in frames {
        ensure!(
            thumbnails.len() < MAX_FRAMES
                && (thumbnails.len() as u64 + 1) * u64::from(width) * u64::from(height)
                    <= MAX_PIXELS,
            "sticker_animation_budget_exceeded"
        );
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "sticker_animation_decode_timeout"
        );
        let frame = frame.map_err(|_| anyhow::anyhow!("sticker_animation_invalid"))?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        ensure!(denominator > 0, "sticker_animation_invalid");
        times.push(duration);
        duration = duration.saturating_add((u64::from(numerator) / u64::from(denominator)).max(10));
        let buffer = frame.into_buffer();
        ensure!(
            buffer.width() <= 1024 && buffer.height() <= 1024,
            "sticker_animation_dimensions_exceeded"
        );
        thumbnails.push(
            DynamicImage::ImageRgba8(buffer)
                .thumbnail(240, 240)
                .into_rgba8(),
        );
    }
    ensure!(!thumbnails.is_empty(), "sticker_animation_empty");
    let mut indices = vec![0usize];
    for fraction in 1..4 {
        let target = duration.saturating_sub(1) * fraction / 3;
        let index = times
            .partition_point(|time| *time <= target)
            .saturating_sub(1);
        if !indices.contains(&index) {
            indices.push(index);
        }
    }
    let samples = indices
        .iter()
        .map(|&index| FrameSample {
            frame: index + 1,
            time_ms: times[index],
        })
        .collect::<Vec<_>>();
    let count = thumbnails.len();
    let columns = if indices.len() > 1 { 2 } else { 1 };
    let rows = indices.len().div_ceil(columns);
    let mut sheet = RgbaImage::from_pixel(
        columns as u32 * 256,
        rows as u32 * 280,
        Rgba([245, 245, 245, 255]),
    );
    for (position, &index) in indices.iter().enumerate() {
        let thumb = &thumbnails[index];
        let x = (position % columns) as u32 * 256 + (256 - thumb.width()) / 2;
        let y = (position / columns) as u32 * 280;
        imageops::overlay(
            &mut sheet,
            thumb,
            i64::from(x),
            i64::from(y + (248 - thumb.height()) / 2),
        );
        label(
            &mut sheet,
            (position % columns) as u32 * 256 + 8,
            y + 256,
            &format!("{} {}ms", index + 1, times[index]),
        );
    }
    Ok(Decoded {info:VisualInfo{mime:mime.into(),width,height,animated:count>1,frame_count:count,duration_ms:duration,samples,
        interpretation:"Sampled composited frames in order; labels show frame number and approximate encoded-delay time. This is not complete motion understanding.".into()},preview:png(&sheet)?})
}
/// Small built-in numeric glyphs avoid a font/subprocess dependency.
fn label(image: &mut RgbaImage, x: u32, y: u32, text: &str) {
    let glyphs: [[u8; 5]; 12] = [
        [31, 17, 17, 17, 31],
        [0, 18, 31, 16, 0],
        [29, 21, 21, 21, 23],
        [21, 21, 21, 21, 31],
        [7, 4, 4, 4, 31],
        [23, 21, 21, 21, 29],
        [31, 21, 21, 21, 29],
        [1, 1, 25, 5, 3],
        [31, 21, 21, 21, 31],
        [23, 21, 21, 21, 31],
        [30, 2, 30, 2, 28],
        [18, 21, 21, 21, 9],
    ];
    for (index, ch) in text.chars().enumerate() {
        let glyph = match ch {
            '0'..='9' => Some((ch as u8 - b'0') as usize),
            'm' => Some(10),
            's' => Some(11),
            _ => None,
        };
        if let Some(glyph) = glyph {
            for (column, bits) in glyphs[glyph].iter().enumerate() {
                for row in 0..5 {
                    if bits & (1 << row) != 0 {
                        let px = x + index as u32 * 12 + column as u32 * 2;
                        let py = y + row * 2;
                        for dx in 0..2 {
                            for dy in 0..2 {
                                if px + dx < image.width() && py + dy < image.height() {
                                    image.put_pixel(px + dx, py + dy, Rgba([30, 30, 30, 255]));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn decode(bytes: &[u8]) -> Result<Decoded> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_BYTES,
        "sticker_file_too_large"
    );
    let mime = super::super::images::sniff_mime(bytes)
        .ok_or_else(|| anyhow::anyhow!("sticker_format_unsupported"))?;
    if mime == "image/gif" {
        let mut decoder = GifDecoder::new(Cursor::new(bytes))
            .map_err(|_| anyhow::anyhow!("sticker_decode_failed"))?;
        decoder.set_limits(limits(1024))?;
        let (w, h) = decoder.dimensions();
        return frames(decoder.into_frames(), w, h, mime);
    }
    if mime == "image/webp" {
        let mut decoder = WebPDecoder::new(Cursor::new(bytes))
            .map_err(|_| anyhow::anyhow!("sticker_decode_failed"))?;
        decoder.set_limits(limits(4096))?;
        if decoder.has_animation() {
            let (w, h) = decoder.dimensions();
            return frames(decoder.into_frames(), w, h, mime);
        }
    }
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::WebP,
        _ => unreachable!(),
    };
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits(4096));
    let image = reader
        .decode()
        .map_err(|_| anyhow::anyhow!("sticker_decode_failed"))?;
    let (w, h) = (image.width(), image.height());
    let preview = image.thumbnail(512, 512).into_rgba8();
    Ok(Decoded {
        info: VisualInfo {
            mime: mime.into(),
            width: w,
            height: h,
            animated: false,
            frame_count: 1,
            duration_ms: 0,
            samples: vec![FrameSample {
                frame: 1,
                time_ms: 0,
            }],
            interpretation: "Static image preview.".into(),
        },
        preview: png(&preview)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gif_preview_samples_frames_but_does_not_replace_original() {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for color in [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]] {
                encoder
                    .encode_frame(image::Frame::from_parts(
                        RgbaImage::from_pixel(8, 8, Rgba(color)),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(100, 1),
                    ))
                    .unwrap();
            }
        }
        let original = bytes.clone();
        let result = decode(&bytes).unwrap();
        assert_eq!(bytes, original);
        assert!(result.info.animated);
        assert_eq!(result.info.frame_count, 3);
        assert_eq!(result.info.duration_ms, 300);
        assert_eq!(result.info.samples.last().unwrap().frame, 3);
        assert!(result.preview.starts_with(b"\x89PNG"));
    }
    #[test]
    fn malformed_and_bomb_dimensions_are_rejected() {
        assert!(decode(b"GIF89a broken").is_err());
        let bytes = vec![0; MAX_BYTES + 1];
        assert!(decode(&bytes).is_err());
    }
}
