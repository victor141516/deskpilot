//! Screen capture with BitBlt and window capture with PrintWindow
//! (PW_RENDERFULLCONTENT renders the window's own pixels even when covered).

use deskpilot_core::model::{Capture, Point, Rect, WindowId};
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC,
    ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

use crate::util::rect;
use crate::win::{existing, frame_bounds};

const PW_RENDERFULLCONTENT: u32 = 2;

/// Copies a bitmap selected in `dc` into RGBA.
unsafe fn read_bitmap(dc: HDC, bmp: HBITMAP, width: i32, height: i32) -> Result<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let lines = unsafe {
        GetDIBits(
            dc,
            bmp,
            0,
            height as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if lines != height {
        return Err(Error::Platform("GetDIBits failed".into()));
    }
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA → RGBA
        px[3] = 255;
    }
    Ok(pixels)
}

pub fn screen(r: Rect) -> Result<Capture> {
    if r.is_empty() {
        return Err(Error::InvalidArgument("empty capture rectangle".into()));
    }
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, r.width, r.height);
        let old = SelectObject(mem, bmp.into());
        let blit = BitBlt(
            mem,
            0,
            0,
            r.width,
            r.height,
            Some(screen),
            r.x,
            r.y,
            SRCCOPY | CAPTUREBLT,
        );
        let result = blit
            .map_err(|e| Error::Platform(format!("BitBlt: {e}")))
            .and_then(|_| read_bitmap(mem, bmp, r.width, r.height));
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        Ok(Capture {
            width: r.width as u32,
            height: r.height as u32,
            rgba: result?,
            origin: Point { x: r.x, y: r.y },
            occlusion_free: false,
        })
    }
}

pub fn window(id: &WindowId) -> Result<Capture> {
    let hwnd = existing(id)?;
    let frame = frame_bounds(hwnd);
    match print_window(hwnd, frame) {
        Ok(cap) if !all_black(&cap.rgba) => Ok(cap),
        // Some renderers (games, protected content) do not support PrintWindow.
        _ => screen(frame),
    }
}

fn print_window(hwnd: HWND, frame: Rect) -> Result<Capture> {
    unsafe {
        let mut wr = RECT::default();
        GetWindowRect(hwnd, &mut wr).map_err(|e| Error::Platform(format!("GetWindowRect: {e}")))?;
        let full = rect(wr);
        if full.is_empty() {
            return Err(Error::Platform("window has no area".into()));
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, full.width, full.height);
        let old = SelectObject(mem, bmp.into());
        let ok = PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();
        let pixels = if ok {
            read_bitmap(mem, bmp, full.width, full.height)
        } else {
            Err(Error::Platform("PrintWindow failed".into()))
        };
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let full_cap = Capture {
            width: full.width as u32,
            height: full.height as u32,
            rgba: pixels?,
            origin: Point {
                x: full.x,
                y: full.y,
            },
            occlusion_free: true,
        };
        // Drop the invisible resize borders.
        deskpilot_core::imaging::crop(&full_cap, frame)
    }
}

fn all_black(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4)
        .step_by(7)
        .all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0)
}
