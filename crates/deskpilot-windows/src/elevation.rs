//! Elevated worker transport: the parent creates a private named pipe, starts
//! `deskpilot worker --endpoint <pipe>` through the UAC "runas" verb and waits
//! for the worker to connect.

use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deskpilot_core::platform::Duplex;
use deskpilot_core::{Error, Result};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

use crate::shell::{HIDDEN, execute};
use crate::util::WideStr;

/// How long the user has to answer the UAC prompt.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);

pub fn spawn_worker(extra_args: &[String]) -> Result<Box<dyn Duplex>> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!(r"\\.\pipe\deskpilot-{}-{nanos}", std::process::id());
    let name_w = WideStr::new(&name);
    let pipe = unsafe {
        CreateNamedPipeW(
            name_w.pcwstr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            1 << 16,
            1 << 16,
            0,
            None,
        )
    };
    if pipe.is_invalid() {
        return Err(Error::Platform(format!("CreateNamedPipe: {:?}", unsafe {
            GetLastError()
        })));
    }
    let close_pipe = || unsafe {
        let _ = CloseHandle(pipe);
    };

    let exe = std::env::current_exe().map_err(Error::platform)?;
    let mut args = vec!["worker".to_string(), "--endpoint".to_string(), name.clone()];
    args.extend_from_slice(extra_args);
    let process = match execute("runas", &exe.to_string_lossy(), &args, None, HIDDEN) {
        Ok(Some(p)) => p,
        Ok(None) => {
            close_pipe();
            return Err(Error::Platform("the elevated worker did not start".into()));
        }
        Err(e) => {
            close_pipe();
            return Err(e);
        }
    };

    let (tx, rx) = mpsc::channel();
    let raw = pipe.0 as usize;
    std::thread::spawn(move || {
        let connected = unsafe { ConnectNamedPipe(HANDLE(raw as *mut _), None) }.is_ok()
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        let _ = tx.send(connected);
    });

    let start = Instant::now();
    let outcome = loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(true) => break Ok(()),
            Ok(false) => {
                break Err(Error::Platform(
                    "the elevated worker could not connect".into(),
                ));
            }
            Err(_) => {}
        }
        if unsafe { WaitForSingleObject(process, 0) } == WAIT_OBJECT_0 {
            let mut code = 0u32;
            let _ = unsafe { GetExitCodeProcess(process, &mut code) };
            break Err(Error::Platform(format!(
                "the elevated worker exited with code {code} before connecting"
            )));
        }
        if start.elapsed() > CONSENT_TIMEOUT {
            break Err(Error::Timeout("no answer to the elevation prompt".into()));
        }
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    if let Err(e) = outcome {
        // Unblock the waiting thread by connecting to our own pipe, then drop everything.
        if let Ok(h) = unsafe {
            CreateFileW(
                name_w.pcwstr(),
                GENERIC_READ.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        } {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        close_pipe();
        return Err(e);
    }
    Ok(Box::new(unsafe { File::from_raw_handle(pipe.0) }))
}

pub fn connect(endpoint: &str) -> Result<Box<dyn Duplex>> {
    // The worker has no business showing a console window.
    unsafe {
        let console = GetConsoleWindow();
        if !console.is_invalid() {
            let _ = ShowWindow(console, SW_HIDE);
        }
    }
    let name = WideStr::new(endpoint);
    let handle = unsafe {
        CreateFileW(
            name.pcwstr(),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| Error::Platform(format!("connect to {endpoint}: {e}")))?;
    Ok(Box::new(unsafe { File::from_raw_handle(handle.0) }))
}
