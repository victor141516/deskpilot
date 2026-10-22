use deskpilot_core::model::{Rect, WindowId};
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Variant::{VARIANT, VariantClear};
use windows::core::PCWSTR;

pub fn hwnd_id(hwnd: HWND) -> WindowId {
    WindowId(format!("0x{:X}", hwnd.0 as usize))
}

pub fn parse_hwnd(id: &WindowId) -> Result<HWND> {
    let text = id.0.trim();
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    let value = usize::from_str_radix(hex, 16)
        .map_err(|_| Error::InvalidArgument(format!("'{text}' is not a window id (0x… hex)")))?;
    Ok(HWND(value as *mut _))
}

pub fn rect(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Keeps a NUL-terminated UTF-16 buffer alive while a `PCWSTR` points into it.
pub struct WideStr(Vec<u16>);

impl WideStr {
    pub fn new(s: &str) -> Self {
        Self(wide(s))
    }
    pub fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub fn win_err(context: &str, e: windows::core::Error) -> Error {
    Error::Platform(format!("{context}: {e}"))
}

/// Owned VARIANT that is cleared on drop.
pub struct Var(pub VARIANT);

impl Drop for Var {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

impl Var {
    pub fn vt(&self) -> u16 {
        unsafe { self.0.Anonymous.Anonymous.vt.0 }
    }
}
