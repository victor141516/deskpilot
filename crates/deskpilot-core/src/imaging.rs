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
/// `scale` folds together the capture's pixel density and any downscaling.
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
    let r = cap
        .screen_rect()
        .intersect(&rect)
        .ok_or_else(|| Error::InvalidArgument("region is outside the captured area".into()))?;
    let s = cap.pixel_scale;
    let px = |v: i32| ((v as f64 * s).round() as i64).max(0) as usize;
    let (ox, oy) = (px(r.x - cap.origin.x), px(r.y - cap.origin.y));
    let w = px(r.width).min(cap.width as usize - ox.min(cap.width as usize));
    let h = px(r.height).min(cap.height as usize - oy.min(cap.height as usize));
    if w == 0 || h == 0 {
        return Err(Error::InvalidArgument("region is outside the captured area".into()));
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = ((oy + row) * cap.width as usize + ox) * 4;
        rgba.extend_from_slice(&cap.rgba[start..start + w * 4]);
    }
    Ok(Capture {
        width: w as u32,
        height: h as u32,
        rgba,
        origin: Point { x: r.x, y: r.y },
        pixel_scale: s,
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
        scale: scale * cap.pixel_scale,
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
        pixel_scale: cap.pixel_scale * width as f64 / cap.width.max(1) as f64,
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
        pixel_scale: 1.0,
        occlusion_free: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(w: u32, h: u32, pixel_scale: f64) -> Capture {
        let rgba = (0..w * h)
            .flat_map(|i| [(i % 256) as u8, 0, 0, 255])
            .collect();
        Capture {
            width: w,
            height: h,
            rgba,
            origin: Point { x: 100, y: 50 },
            pixel_scale,
            occlusion_free: true,
        }
    }

    #[test]
    fn crop_uses_screen_coordinates() {
        let c = crop(&capture(10, 10, 1.0), Rect::new(102, 53, 3, 2)).unwrap();
        assert_eq!(
            (c.width, c.height, c.origin),
            (3, 2, Point { x: 102, y: 53 })
        );
        // first pixel of the crop is source pixel (2, 3) → index 32
        assert_eq!(c.rgba[0], 32);
    }

    #[test]
    fn crop_on_a_dense_display_maps_screen_units_to_pixels() {
        // 20x20 pixels covering 10x10 screen units.
        let c = crop(&capture(20, 20, 2.0), Rect::new(102, 53, 3, 2)).unwrap();
        assert_eq!(
            (c.width, c.height, c.origin, c.screen_rect()),
            (6, 4, Point { x: 102, y: 53 }, Rect::new(102, 53, 3, 2))
        );
        // first pixel is source pixel (4, 6) → index 6 * 20 + 4 = 124
        assert_eq!(c.rgba[0], 124);
        // A rectangle hanging off the edge is clipped to the image.
        let edge = crop(&capture(20, 20, 2.0), Rect::new(108, 58, 10, 10)).unwrap();
        assert_eq!((edge.width, edge.height), (4, 4));
    }

    #[test]
    fn encode_downscales_and_reports_mapping() {
        let (img, map) = encode(&capture(400, 200, 1.0), Some(100), ImageFormat::Png).unwrap();
        assert_eq!((img.width, img.height), (100, 50));
        assert_eq!(map.scale, 0.25);
        assert_eq!(map.origin, Point { x: 100, y: 50 });
        let (img, map) = encode(&capture(40, 20, 1.0), Some(100), ImageFormat::Jpeg).unwrap();
        assert_eq!((img.width, map.scale, img.mime), (40, 1.0, "image/jpeg"));
    }

    #[test]
    fn mapping_scale_includes_pixel_density() {
        // 400 pixels over 200 screen units, downscaled to 100: image_px / 0.5 = screen units.
        let (_, map) = encode(&capture(400, 200, 2.0), Some(100), ImageFormat::Png).unwrap();
        assert_eq!(map.scale, 0.5);
        let (_, map) = encode(&capture(40, 20, 2.0), None, ImageFormat::Png).unwrap();
        assert_eq!(map.scale, 2.0);
    }
}
