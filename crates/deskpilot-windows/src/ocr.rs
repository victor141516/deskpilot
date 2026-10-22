//! Text recognition with Windows.Media.Ocr (installed OCR languages only).

use deskpilot_core::imaging;
use deskpilot_core::model::{Capture, OcrLine, OcrWord, Rect};
use deskpilot_core::{Error, Result};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::core::HSTRING;

use crate::util::win_err;

/// Returns bounds relative to the image (the core adds the capture origin).
pub fn recognize(image: &Capture, language: Option<&str>) -> Result<Vec<OcrLine>> {
    let engine = match language {
        Some(tag) => {
            let lang = Language::CreateLanguage(&HSTRING::from(tag))
                .map_err(|e| Error::InvalidArgument(format!("language '{tag}': {e}")))?;
            if !OcrEngine::IsLanguageSupported(&lang).unwrap_or(false) {
                return Err(Error::Unsupported(format!(
                    "no OCR language pack installed for '{tag}'"
                )));
            }
            OcrEngine::TryCreateFromLanguage(&lang)
        }
        None => OcrEngine::TryCreateFromUserProfileLanguages(),
    }
    .map_err(|e| Error::Unsupported(format!("OCR engine unavailable: {e}")))?;

    let max =
        OcrEngine::MaxImageDimension().map_err(|e| win_err("OcrEngine::MaxImageDimension", e))?;
    let scale = imaging::fit_scale(image.width, image.height, Some(max));
    let scaled;
    let img = if scale < 1.0 {
        scaled = imaging::resized(
            image,
            (image.width as f64 * scale) as u32,
            (image.height as f64 * scale) as u32,
        )?;
        &scaled
    } else {
        image
    };

    let bgra: Vec<u8> = img
        .rgba
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();
    let buffer = CryptographicBuffer::CreateFromByteArray(&bgra)
        .map_err(|e| win_err("CreateFromByteArray", e))?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        img.width as i32,
        img.height as i32,
    )
    .map_err(|e| win_err("SoftwareBitmap", e))?;
    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|op| op.join())
        .map_err(|e| win_err("OCR", e))?;

    let back = |v: f32| (v as f64 / scale).round() as i32;
    let mut lines = Vec::new();
    for line in result.Lines().map_err(|e| win_err("OCR lines", e))? {
        let mut words = Vec::new();
        for word in line.Words().map_err(|e| win_err("OCR words", e))? {
            let r = word
                .BoundingRect()
                .map_err(|e| win_err("OCR word bounds", e))?;
            words.push(OcrWord {
                text: word.Text().map(|t| t.to_string()).unwrap_or_default(),
                bounds: Rect::new(back(r.X), back(r.Y), back(r.Width), back(r.Height)),
            });
        }
        let bounds = words.iter().map(|w| w.bounds).reduce(|a, b| {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            Rect::new(
                x,
                y,
                (a.x + a.width).max(b.x + b.width) - x,
                (a.y + a.height).max(b.y + b.height) - y,
            )
        });
        lines.push(OcrLine {
            text: line.Text().map(|t| t.to_string()).unwrap_or_default(),
            bounds: bounds.unwrap_or(Rect::new(0, 0, 0, 0)),
            words,
        });
    }
    Ok(lines)
}
