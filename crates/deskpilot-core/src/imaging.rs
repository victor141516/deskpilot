//! Encoding and scaling of captures. Images are the most expensive output for
//! an agent, so every capture is downscaled to `max_size` unless asked not to.

use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat as ImgFormat, RgbaImage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{Capture, Point, Rect};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
}

#[derive(Debug, Clone)]
pub struct EncodedImage {
    pub mime: &'static str,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Where an encoded image sits on screen: `screen = origin + image_px / scale`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImageMapping {
    pub origin: Point,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub occlusion_free: bool,
}

pub fn to_image(cap: &Capture) -> Result<RgbaImage> {
    RgbaImage::from_raw(cap.width, cap.height, cap.rgba.clone())
        .ok_or_else(|| Error::Platform("capture buffer has the wrong size".into()))
}

/// Crops a capture to `rect` (screen coordinates).
pub fn crop(cap: &Capture, rect: Rect) -> Result<Capture> {
    let full = Rect::new(
        cap.origin.x,
        cap.origin.y,
        cap.width as i32,
        cap.height as i32,
    );
    let r = full
        .intersect(&rect)
        .ok_or_else(|| Error::InvalidArgument("region is outside the captured area".into()))?;
    let (ox, oy) = ((r.x - full.x) as usize, (r.y - full.y) as usize);
    let mut rgba = Vec::with_capacity(r.width as usize * r.height as usize * 4);
    for row in 0..r.height as usize {
        let start = ((oy + row) * cap.width as usize + ox) * 4;
        rgba.extend_from_slice(&cap.rgba[start..start + r.width as usize * 4]);
    }
    Ok(Capture {
        width: r.width as u32,
        height: r.height as u32,
        rgba,
        origin: Point { x: r.x, y: r.y },
        occlusion_free: cap.occlusion_free,
    })
}

/// Scale factor that fits the longest side into `max_size` (never upscales).
pub fn fit_scale(width: u32, height: u32, max_size: Option<u32>) -> f64 {
    match max_size {
        Some(max) if max > 0 && width.max(height) > max => max as f64 / width.max(height) as f64,
        _ => 1.0,
    }
}

pub fn encode(
    cap: &Capture,
    max_size: Option<u32>,
    format: ImageFormat,
) -> Result<(EncodedImage, ImageMapping)> {
    let scale = fit_scale(cap.width, cap.height, max_size);
    let mut img = to_image(cap)?;
    if scale < 1.0 {
        let w = ((cap.width as f64 * scale).round() as u32).max(1);
        let h = ((cap.height as f64 * scale).round() as u32).max(1);
        img = image::imageops::resize(&img, w, h, FilterType::Triangle);
    }
    let (width, height) = img.dimensions();
    let mut data = Vec::new();
    let mime = match format {
        ImageFormat::Png => {
            DynamicImage::ImageRgba8(img)
                .write_to(&mut Cursor::new(&mut data), ImgFormat::Png)
                .map_err(Error::platform)?;
            "image/png"
        }
        ImageFormat::Jpeg => {
            DynamicImage::ImageRgba8(img)
                .to_rgb8()
                .write_to(&mut Cursor::new(&mut data), ImgFormat::Jpeg)
                .map_err(Error::platform)?;
            "image/jpeg"
        }
    };
    let mapping = ImageMapping {
        origin: cap.origin,
        scale,
        width,
        height,
        source_width: cap.width,
        source_height: cap.height,
        occlusion_free: cap.occlusion_free,
    };
    Ok((
        EncodedImage {
            mime,
            data,
            width,
            height,
        },
        mapping,
    ))
}

/// Resampled copy (adapters use it for engines with a size limit, e.g. OCR).
pub fn resized(cap: &Capture, width: u32, height: u32) -> Result<Capture> {
    let img = image::imageops::resize(
        &to_image(cap)?,
        width.max(1),
        height.max(1),
        FilterType::Triangle,
    );
    let (width, height) = img.dimensions();
    Ok(Capture {
        width,
        height,
        rgba: img.into_raw(),
        origin: cap.origin,
        occlusion_free: cap.occlusion_free,
    })
}

pub fn decode_file(path: &std::path::Path) -> Result<Capture> {
    let img = image::open(path)
        .map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.display())))?
        .to_rgba8();
    let (width, height) = img.dimensions();
    Ok(Capture {
        width,
        height,
        rgba: img.into_raw(),
        origin: Point { x: 0, y: 0 },
        occlusion_free: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(w: u32, h: u32) -> Capture {
        let rgba = (0..w * h)
            .flat_map(|i| [(i % 256) as u8, 0, 0, 255])
            .collect();
        Capture {
            width: w,
            height: h,
            rgba,
            origin: Point { x: 100, y: 50 },
            occlusion_free: true,
        }
    }

    #[test]
    fn crop_uses_screen_coordinates() {
        let c = crop(&capture(10, 10), Rect::new(102, 53, 3, 2)).unwrap();
        assert_eq!(
            (c.width, c.height, c.origin),
            (3, 2, Point { x: 102, y: 53 })
        );
        // first pixel of the crop is source pixel (2, 3) → index 32
        assert_eq!(c.rgba[0], 32);
    }

    #[test]
    fn encode_downscales_and_reports_mapping() {
        let (img, map) = encode(&capture(400, 200), Some(100), ImageFormat::Png).unwrap();
        assert_eq!((img.width, img.height), (100, 50));
        assert_eq!(map.scale, 0.25);
        assert_eq!(map.origin, Point { x: 100, y: 50 });
        let (img, map) = encode(&capture(40, 20), Some(100), ImageFormat::Jpeg).unwrap();
        assert_eq!((img.width, map.scale, img.mime), (40, 1.0, "image/jpeg"));
    }
}
