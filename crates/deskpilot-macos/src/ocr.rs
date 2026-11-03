//! Vision OCR (`VNRecognizeTextRequest`).
use deskpilot_core::model::{Capture, OcrLine, OcrWord, Rect};
use deskpilot_core::{Error, Result};
use objc2::AnyThread;
use objc2::rc::autoreleasepool;
use objc2_core_foundation::{CFData, CGRect};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
    CGImageAlphaInfo, CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSArray, NSDictionary, NSRange, NSString};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedText, VNRequest,
    VNRequestTextRecognitionLevel,
};

/// Returns bounds in image pixels (the core adds the capture origin/scale).
pub fn recognize(image: &Capture, language: Option<&str>) -> Result<Vec<OcrLine>> {
    let (w, h) = (image.width as usize, image.height as usize);
    if w == 0 || h == 0 || image.rgba.len() < w * h * 4 {
        return Err(Error::InvalidArgument(format!(
            "image is {w}x{h} with {} bytes of RGBA",
            image.rgba.len()
        )));
    }
    // Vision misses small text (toolbar labels at 1×): enlarge short images.
    // Boxes come back normalized, so they still map onto the original size.
    const MIN_SIDE: u32 = 400;
    let factor = (MIN_SIDE as f64 / image.width.min(image.height) as f64).clamp(1.0, 4.0);
    let upscaled = (factor > 1.0)
        .then(|| {
            deskpilot_core::imaging::resized(
                image,
                (image.width as f64 * factor).round() as u32,
                (image.height as f64 * factor).round() as u32,
            )
        })
        .transpose()?;
    let source = upscaled.as_ref().unwrap_or(image);
    autoreleasepool(|_| {
        let cg = cg_image(source)?;
        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);
        match language {
            Some(tag) => {
                let supported = unsafe { request.supportedRecognitionLanguagesAndReturnError() }
                    .map(|langs| langs.iter().map(|l| l.to_string()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let wanted = tag.replace('_', "-");
                let lang = supported
                    .iter()
                    .find(|l| l.eq_ignore_ascii_case(&wanted))
                    .or_else(|| {
                        // "en" → "en-US", "zh" → "zh-Hans", …
                        supported.iter().find(|l| {
                            l.split('-')
                                .next()
                                .is_some_and(|p| p.eq_ignore_ascii_case(&wanted))
                        })
                    })
                    .ok_or_else(|| {
                        Error::Unsupported(format!(
                            "Vision cannot recognize '{tag}' (supported: {})",
                            supported.join(", ")
                        ))
                    })?;
                request.setRecognitionLanguages(&NSArray::from_retained_slice(&[
                    NSString::from_str(lang),
                ]));
            }
            None => request.setAutomaticallyDetectsLanguage(true),
        }

        let handler = unsafe {
            VNImageRequestHandler::initWithCGImage_options(
                VNImageRequestHandler::alloc(),
                &cg,
                &NSDictionary::new(),
            )
        };
        let as_request: &VNRequest = &request;
        handler
            .performRequests_error(&NSArray::from_slice(&[as_request]))
            .map_err(|e| Error::Platform(format!("Vision OCR: {}", e.localizedDescription())))?;

        let to_px = |b: CGRect| pixel_rect(b, w as f64, h as f64);
        let mut lines = Vec::new();
        for obs in request.results().unwrap_or_default().iter() {
            let Some(top) = obs.topCandidates(1).firstObject() else {
                continue;
            };
            let text = top.string().to_string();
            if text.trim().is_empty() {
                continue;
            }
            lines.push(OcrLine {
                words: words(&top, &text, &to_px),
                bounds: to_px(unsafe { obs.boundingBox() }),
                text,
            });
        }
        Ok(lines)
    })
}

/// Whitespace-separated words of `text`, each with its own box.
fn words(top: &VNRecognizedText, text: &str, to_px: &dyn Fn(CGRect) -> Rect) -> Vec<OcrWord> {
    let mut out = Vec::new();
    // NSRange counts UTF-16 code units.
    let mut utf16 = 0usize;
    let mut start: Option<(usize, usize)> = None; // (byte, utf16)
    let mut push = |from: (usize, usize), to: (usize, usize)| {
        let range = NSRange {
            location: from.1,
            length: to.1 - from.1,
        };
        if let Ok(rect) = unsafe { top.boundingBoxForRange_error(range) } {
            out.push(OcrWord {
                text: text[from.0..to.0].to_string(),
                bounds: to_px(unsafe { rect.boundingBox() }),
            });
        }
    };
    for (byte, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if let Some(s) = start.take() {
                push(s, (byte, utf16));
            }
        } else if start.is_none() {
            start = Some((byte, utf16));
        }
        utf16 += ch.len_utf16();
    }
    if let Some(s) = start {
        push(s, (text.len(), utf16));
    }
    out
}

/// Vision normalized box (origin bottom-left) → image pixels (top-left), rounded outward.
fn pixel_rect(b: CGRect, w: f64, h: f64) -> Rect {
    let x1 = (b.origin.x * w).floor().clamp(0.0, w);
    let x2 = ((b.origin.x + b.size.width) * w).ceil().clamp(0.0, w);
    let y1 = ((1.0 - b.origin.y - b.size.height) * h).floor().clamp(0.0, h);
    let y2 = ((1.0 - b.origin.y) * h).ceil().clamp(0.0, h);
    Rect::new(x1 as i32, y1 as i32, (x2 - x1) as i32, (y2 - y1) as i32)
}

fn cg_image(image: &Capture) -> Result<objc2_core_foundation::CFRetained<CGImage>> {
    let (w, h) = (image.width as usize, image.height as usize);
    let data = CFData::from_bytes(&image.rgba[..w * h * 4]);
    let provider = CGDataProvider::with_cf_data(Some(&data))
        .ok_or_else(|| Error::Platform("CGDataProviderCreateWithCFData failed".into()))?;
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or_else(|| Error::Platform("CGColorSpaceCreateWithName(sRGB) failed".into()))?;
    // SAFETY: `decode` may be null; the provider holds w*h*4 bytes.
    unsafe {
        CGImage::new(
            w,
            h,
            8,
            32,
            w * 4,
            Some(&space),
            CGBitmapInfo(CGImageAlphaInfo::Last.0 | CGImageByteOrderInfo::Order32Big.0),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
    .ok_or_else(|| Error::Platform(format!("CGImageCreate {w}x{h} failed")))
}
