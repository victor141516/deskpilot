//! Clipboard text (CF_UNICODETEXT) and images (CF_DIB).

use std::thread::sleep;
use std::time::Duration;

use deskpilot_core::model::{Capture, Point};
use deskpilot_core::platform::ClipboardContent;
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::{BI_BITFIELDS, BI_RGB, BITMAPINFOHEADER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};

struct Open;

impl Open {
    fn new() -> Result<Self> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Open);
            }
            sleep(Duration::from_millis(25)); // another process holds it
        }
        Err(Error::Platform("the clipboard is busy".into()))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

pub fn get() -> Result<ClipboardContent> {
    let _open = Open::new()?;
    let mut content = ClipboardContent::default();
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
            if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) {
                let g = HGLOBAL(h.0);
                let ptr = GlobalLock(g) as *const u16;
                if !ptr.is_null() {
                    let max = GlobalSize(g) / 2;
                    let len = (0..max).take_while(|i| *ptr.add(*i) != 0).count();
                    content.text = Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                        ptr, len,
                    )));
                    let _ = GlobalUnlock(g);
                }
            }
        }
        if IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok() {
            if let Ok(h) = GetClipboardData(CF_DIB.0 as u32) {
                let g = HGLOBAL(h.0);
                let ptr = GlobalLock(g) as *const u8;
                if !ptr.is_null() {
                    let bytes = std::slice::from_raw_parts(ptr, GlobalSize(g));
                    content.image = dib_to_capture(bytes);
                    let _ = GlobalUnlock(g);
                }
            }
        }
    }
    Ok(content)
}

fn dib_to_capture(bytes: &[u8]) -> Option<Capture> {
    if bytes.len() < size_of::<BITMAPINFOHEADER>() {
        return None;
    }
    let header: BITMAPINFOHEADER = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const _) };
    let width = header.biWidth;
    let top_down = header.biHeight < 0;
    let height = header.biHeight.abs();
    let bpp = header.biBitCount as usize;
    if width <= 0 || height <= 0 || !(bpp == 24 || bpp == 32) {
        return None;
    }
    let mut offset = header.biSize as usize;
    if header.biCompression == BI_BITFIELDS.0 && header.biSize == 40 {
        offset += 12; // three colour masks follow the header
    } else if header.biCompression != BI_RGB.0 && header.biCompression != BI_BITFIELDS.0 {
        return None;
    }
    let stride = (width as usize * bpp).div_ceil(32) * 4;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let row = if top_down { y } else { height as usize - 1 - y };
        let start = offset + row * stride;
        let line = bytes.get(start..start + width as usize * bpp / 8)?;
        for px in line.chunks_exact(bpp / 8) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    Some(Capture {
        width: width as u32,
        height: height as u32,
        rgba,
        origin: Point { x: 0, y: 0 },
        occlusion_free: true,
    })
}

unsafe fn put(format: u32, data: &[u8]) -> Result<()> {
    unsafe {
        let g = GlobalAlloc(GMEM_MOVEABLE, data.len())
            .map_err(|e| Error::Platform(format!("GlobalAlloc: {e}")))?;
        let ptr = GlobalLock(g) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(g));
            return Err(Error::Platform("GlobalLock failed".into()));
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        let _ = GlobalUnlock(g);
        if let Err(e) = SetClipboardData(format, Some(HANDLE(g.0))) {
            let _ = GlobalFree(Some(g));
            return Err(Error::Platform(format!("SetClipboardData: {e}")));
        }
        Ok(()) // the clipboard owns the memory now
    }
}

pub fn set(content: &ClipboardContent) -> Result<()> {
    let _open = Open::new()?;
    unsafe {
        EmptyClipboard().map_err(|e| Error::Platform(format!("EmptyClipboard: {e}")))?;
        if let Some(text) = &content.text {
            let bytes: Vec<u8> = text
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect();
            put(CF_UNICODETEXT.0 as u32, &bytes)?;
        }
        if let Some(img) = &content.image {
            let header = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: img.width as i32,
                biHeight: img.height as i32, // bottom-up
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            let mut bytes = Vec::with_capacity(size_of::<BITMAPINFOHEADER>() + img.rgba.len());
            bytes.extend_from_slice(std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                size_of::<BITMAPINFOHEADER>(),
            ));
            for row in (0..img.height as usize).rev() {
                let line =
                    &img.rgba[row * img.width as usize * 4..(row + 1) * img.width as usize * 4];
                for px in line.chunks_exact(4) {
                    bytes.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            }
            put(CF_DIB.0 as u32, &bytes)?;
        }
    }
    Ok(())
}
