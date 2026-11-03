//! NSPasteboard: plain text and images (PNG, TIFF) as RGBA8.

use std::ffi::c_void;

use deskpilot_core::model::{Capture, Point};
use deskpilot_core::platform::ClipboardContent;
use deskpilot_core::{Error, Result};
use objc2::AnyThread;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageFileType, NSBitmapImageRep, NSDeviceRGBColorSpace, NSPasteboard,
    NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSData, NSDictionary, NSString};

pub fn get() -> Result<ClipboardContent> {
    autoreleasepool(|_| {
        let pb = NSPasteboard::generalPasteboard();
        let text = unsafe { pb.stringForType(NSPasteboardTypeString) }.map(|s| s.to_string());
        let image = [unsafe { NSPasteboardTypePNG }, unsafe { NSPasteboardTypeTIFF }]
            .into_iter()
            .find_map(|ty| pb.dataForType(ty))
            .map(|data| decode(&data))
            .transpose()?;
        Ok(ClipboardContent { text, image })
    })
}

pub fn set(content: &ClipboardContent) -> Result<()> {
    autoreleasepool(|_| {
        let png = content.image.as_ref().map(encode_png).transpose()?;
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        if let Some(text) = &content.text {
            if !pb.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
            {
                return Err(Error::Platform("NSPasteboard rejected the text".into()));
            }
        }
        if let Some(png) = png {
            if !pb.setData_forType(Some(&png), unsafe { NSPasteboardTypePNG }) {
                return Err(Error::Platform("NSPasteboard rejected the image".into()));
            }
        }
        Ok(())
    })
}

/// Decodes PNG/TIFF bytes into straight-alpha RGBA8 at native pixel size.
fn decode(data: &NSData) -> Result<Capture> {
    let rep = NSBitmapImageRep::imageRepWithData(data)
        .ok_or_else(|| Error::Platform("the clipboard image could not be decoded".into()))?;
    let image = rep
        .CGImage()
        .ok_or_else(|| Error::Platform("the clipboard image has no bitmap".into()))?;
    let width = CGImage::width(Some(&image));
    let height = CGImage::height(Some(&image));
    if width == 0 || height == 0 {
        return Err(Error::Platform("the clipboard image is empty".into()));
    }
    let mut rgba = vec![0u8; width * height * 4];
    let space = CGColorSpace::new_device_rgb()
        .ok_or_else(|| Error::Platform("CGColorSpaceCreateDeviceRGB failed".into()))?;
    // CGBitmapContext only supports premultiplied alpha; un-premultiply below.
    // Byte order default (0): bytes in memory are R, G, B, A.
    let info = CGImageAlphaInfo::PremultipliedLast.0;
    let ctx = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr() as *mut c_void,
            width,
            height,
            8,
            width * 4,
            Some(&space),
            info,
        )
    }
    .ok_or_else(|| Error::Platform("CGBitmapContextCreate failed".into()))?;
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width as f64, height as f64),
    );
    CGContext::draw_image(Some(&ctx), rect, Some(&image));
    drop(ctx);
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(Capture {
        width: width as u32,
        height: height as u32,
        rgba,
        origin: Point { x: 0, y: 0 },
        pixel_scale: 1.0,
        occlusion_free: true,
    })
}

fn encode_png(img: &Capture) -> Result<objc2::rc::Retained<NSData>> {
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 || img.rgba.len() != w * h * 4 {
        return Err(Error::InvalidArgument(format!(
            "image buffer of {} bytes does not match {}x{} RGBA",
            img.rgba.len(),
            img.width,
            img.height
        )));
    }
    // With null planes the rep allocates (and owns) its own buffer.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w as isize,
            h as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied,
            (w * 4) as isize,
            32,
        )
    }
    .ok_or_else(|| Error::Platform("NSBitmapImageRep allocation failed".into()))?;
    let dst = rep.bitmapData();
    let stride = rep.bytesPerRow() as usize;
    if dst.is_null() || stride < w * 4 {
        return Err(Error::Platform("NSBitmapImageRep has no pixel buffer".into()));
    }
    for row in 0..h {
        // SAFETY: the rep owns `stride * h` bytes; each row copy stays in bounds.
        unsafe {
            std::ptr::copy_nonoverlapping(
                img.rgba.as_ptr().add(row * w * 4),
                dst.add(row * stride),
                w * 4,
            );
        }
    }
    unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or_else(|| Error::Platform("PNG encoding failed".into()))
}
