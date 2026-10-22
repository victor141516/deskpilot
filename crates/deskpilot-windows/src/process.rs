//! Process names, image paths and elevation.

use std::collections::HashMap;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

use crate::util::from_wide;

#[derive(Debug, Clone, Default)]
pub struct ProcessInfo {
    pub name: String,
    pub exe_path: Option<String>,
    pub elevated: Option<bool>,
}

/// Per-call cache: one snapshot for names, one OpenProcess per pid.
#[derive(Default)]
pub struct ProcessTable {
    names: Option<HashMap<u32, String>>,
    cache: HashMap<u32, ProcessInfo>,
}

impl ProcessTable {
    pub fn get(&mut self, pid: u32) -> ProcessInfo {
        if let Some(info) = self.cache.get(&pid) {
            return info.clone();
        }
        let (exe_path, elevated) = query(pid);
        let name = exe_path
            .as_deref()
            .map(stem)
            .or_else(|| self.names().get(&pid).map(|n| stem(n)))
            .unwrap_or_default();
        let info = ProcessInfo {
            name,
            exe_path,
            elevated,
        };
        self.cache.insert(pid, info.clone());
        info
    }

    fn names(&mut self) -> &HashMap<u32, String> {
        self.names.get_or_insert_with(snapshot_names)
    }
}

fn stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    file.strip_suffix(".exe")
        .or_else(|| file.strip_suffix(".EXE"))
        .unwrap_or(file)
        .to_string()
}

fn snapshot_names() -> HashMap<u32, String> {
    let mut names = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return names;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                names.insert(entry.th32ProcessID, from_wide(&entry.szExeFile));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    names
}

fn query(pid: u32) -> (Option<String>, Option<bool>) {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return (None, None);
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let path = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()
        .map(|_| String::from_utf16_lossy(&buf[..len as usize]));
        let elevated = token_elevated(handle);
        let _ = CloseHandle(handle);
        (path, elevated)
    }
}

fn token_elevated(process: HANDLE) -> Option<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        ok.ok()?;
        Some(elevation.TokenIsElevated != 0)
    }
}

pub fn current_elevated() -> bool {
    unsafe { token_elevated(GetCurrentProcess()).unwrap_or(false) }
}
