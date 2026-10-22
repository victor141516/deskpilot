//! Launching through the shell (paths, documents, URIs, control panel items,
//! `shell:AppsFolder\<AUMID>`), optionally elevated.

use deskpilot_core::platform::{LaunchResult, LaunchSpec};
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE};
use windows::Win32::System::Threading::GetProcessId;
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{SHOW_WINDOW_CMD, SW_HIDE, SW_SHOWNORMAL};
use windows::core::PCWSTR;

use crate::util::WideStr;

/// Quotes one argument for the Windows command line (CommandLineToArgvW rules).
pub fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Runs ShellExecuteEx and returns the process handle when there is one.
pub fn execute(
    verb: &str,
    file: &str,
    args: &[String],
    cwd: Option<&str>,
    show: SHOW_WINDOW_CMD,
) -> Result<Option<HANDLE>> {
    let verb_w = WideStr::new(verb);
    let file_w = WideStr::new(file);
    let params = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    let params_w = WideStr::new(&params);
    let cwd_w = cwd.map(WideStr::new);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb_w.pcwstr(),
        lpFile: file_w.pcwstr(),
        lpParameters: if params.is_empty() {
            PCWSTR::null()
        } else {
            params_w.pcwstr()
        },
        lpDirectory: cwd_w
            .as_ref()
            .map(WideStr::pcwstr)
            .unwrap_or(PCWSTR::null()),
        nShow: show.0,
        ..Default::default()
    };
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok((!info.hProcess.is_invalid()).then_some(info.hProcess)),
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Err(Error::Cancelled(format!(
            "{verb} {file}: the user declined"
        ))),
        Err(e) => Err(Error::Platform(format!("ShellExecuteEx({file}): {e}"))),
    }
}

pub fn launch(spec: &LaunchSpec) -> Result<LaunchResult> {
    let handle = execute(
        "open",
        &spec.target,
        &spec.args,
        spec.cwd.as_deref(),
        SW_SHOWNORMAL,
    )?;
    let pid = handle.map(|h| unsafe {
        let pid = GetProcessId(h);
        let _ = CloseHandle(h);
        pid
    });
    Ok(LaunchResult {
        pid: pid.filter(|p| *p != 0),
    })
}

pub const HIDDEN: SHOW_WINDOW_CMD = SW_HIDE;

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quotes_like_command_line_to_argv() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(""), "\"\"");
    }
}
