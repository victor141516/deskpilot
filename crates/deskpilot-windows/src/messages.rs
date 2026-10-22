//! Window-message strategy for classic Win32 controls: no focus change and no
//! real input. Only well-defined control messages are used.

use deskpilot_core::model::{ElementInfo, Role};
use deskpilot_core::platform::MessageOp;
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::TCM_SETCURFOCUS;
use windows::Win32::UI::WindowsAndMessaging::{
    BM_CLICK, PostMessageW, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTEXT,
};

use crate::uia::Uia;
use crate::util::{WideStr, parse_hwnd};
use crate::win::class_name;

fn send(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Result<usize> {
    let mut result = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            msg,
            wparam,
            lparam,
            SMTO_ABORTIFHUNG,
            3_000,
            Some(&mut result),
        )
    };
    if ok.0 == 0 {
        // UIPI blocks messages to higher-integrity windows with ERROR_ACCESS_DENIED.
        return Err(Error::Platform(format!(
            "message 0x{msg:X} was not delivered: {:?}",
            unsafe { GetLastError() }
        )));
    }
    Ok(result)
}

pub fn action(uia: &Uia, element: &ElementInfo, op: &MessageOp) -> Result<()> {
    let own = element.native_window.as_ref().map(parse_hwnd).transpose()?;
    let own_class = own.map(class_name).unwrap_or_default();
    match op {
        MessageOp::Press if own_class.eq_ignore_ascii_case("Button") => {
            let hwnd = own.expect("class implies a window");
            // Posted: the click may open a modal dialog that would block SendMessage.
            unsafe { PostMessageW(Some(hwnd), BM_CLICK, WPARAM(0), LPARAM(0)) }
                .map_err(|e| Error::Platform(format!("BM_CLICK: {e}")))
        }
        MessageOp::SetValue(text)
            if own_class.eq_ignore_ascii_case("Edit")
                || own_class.to_ascii_lowercase().starts_with("richedit") =>
        {
            let hwnd = own.expect("class implies a window");
            let w = WideStr::new(text);
            send(hwnd, WM_SETTEXT, WPARAM(0), LPARAM(w.pcwstr().0 as isize)).map(|_| ())
        }
        MessageOp::Select | MessageOp::Press if element.role == Role::TabItem => {
            let (tab_control, index) = uia
                .parent_hwnd_and_index(&element.id)?
                .ok_or_else(|| Error::Unsupported("tab item has no native tab control".into()))?;
            if class_name(tab_control) != "SysTabControl32" {
                return Err(Error::Unsupported(format!(
                    "tab control class is {}",
                    class_name(tab_control)
                )));
            }
            // Moving the focus selects the tab and notifies the parent (TCN_SELCHANGING/TCN_SELCHANGE).
            send(tab_control, TCM_SETCURFOCUS, WPARAM(index), LPARAM(0)).map(|_| ())
        }
        _ => Err(Error::Unsupported(format!(
            "no window message for {op:?} on {} (class '{}')",
            element.role.as_str(),
            if own_class.is_empty() {
                "-"
            } else {
                &own_class
            }
        ))),
    }
}
