//! NSWorkspace launching: URLs, app bundles, documents, bundle ids, app names
//! and plain executables.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use deskpilot_core::platform::{LaunchResult, LaunchSpec};
use deskpilot_core::{Error, Result};
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
use objc2_foundation::{NSArray, NSError, NSString, NSURL};

const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

const APP_DIRS: [&str; 4] = [
    "/Applications",
    "/System/Applications",
    "/System/Applications/Utilities",
    "/Applications/Utilities",
];

pub fn launch(spec: &LaunchSpec) -> Result<LaunchResult> {
    let target = spec.target.trim();
    if target.is_empty() {
        return Err(Error::InvalidArgument("empty launch target".into()));
    }
    if has_url_scheme(target) {
        return autoreleasepool(|_| open_url(target));
    }
    let path = expand_home(target);
    if path.exists() {
        if is_app_bundle(&path) {
            return open_app(&path, &spec.args);
        }
        if is_executable_file(&path) {
            return spawn(&path, spec);
        }
        return autoreleasepool(|_| open_document(&path));
    }
    if looks_like_bundle_id(target) {
        let url = autoreleasepool(|_| {
            NSWorkspace::sharedWorkspace()
                .URLForApplicationWithBundleIdentifier(&NSString::from_str(target))
                .and_then(|u| u.path())
                .map(|p| PathBuf::from(p.to_string()))
        });
        if let Some(app) = url {
            return open_app(&app, &spec.args);
        }
    }
    if !target.contains('/') {
        if let Some(app) = find_app_by_name(target) {
            return open_app(&app, &spec.args);
        }
        if let Some(exe) = find_on_path(target) {
            return spawn(&exe, spec);
        }
    }
    Err(Error::NotFound(format!(
        "'{target}' is not a URL, existing path, application bundle id, application name or executable on PATH"
    )))
}

/// `scheme:rest` with an RFC 3986 scheme of at least two characters.
fn has_url_scheme(s: &str) -> bool {
    let Some((scheme, _)) = s.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    scheme.len() >= 2
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn expand_home(s: &str) -> PathBuf {
    match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ if s == "~" => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
        _ => PathBuf::from(s),
    }
}

fn is_app_bundle(p: &Path) -> bool {
    p.is_dir()
        && p.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("app"))
}

fn is_executable_file(p: &Path) -> bool {
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn looks_like_bundle_id(s: &str) -> bool {
    s.contains('.')
        && !s.starts_with('.')
        && !s.ends_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn find_app_by_name(name: &str) -> Option<PathBuf> {
    let lower = name.to_lowercase();
    let wanted = if lower.ends_with(".app") {
        lower
    } else {
        format!("{lower}.app")
    };
    let home_apps = std::env::var_os("HOME").map(|h| Path::new(&h).join("Applications"));
    APP_DIRS
        .iter()
        .map(PathBuf::from)
        .chain(home_apps)
        .find_map(|dir| {
            std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
                let file_name = entry.file_name();
                (file_name.to_string_lossy().to_lowercase() == wanted)
                    .then(|| entry.path())
                    .filter(|p| is_app_bundle(p))
            })
        })
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| is_executable_file(p))
}

/// Runs `start` with a completion handler and waits for its result. The
/// handler is invoked on a background queue, so the main run loop need not run.
fn wait_open(
    what: &str,
    start: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSRunningApplication, *mut NSError)>),
) -> Result<LaunchResult> {
    let (tx, rx) = mpsc::channel::<std::result::Result<Option<u32>, String>>();
    let handler = RcBlock::new(move |app: *mut NSRunningApplication, err: *mut NSError| {
        // SAFETY: AppKit passes valid (possibly null) object pointers.
        let outcome = match unsafe { err.as_ref() } {
            Some(e) => Err(e.localizedDescription().to_string()),
            None => Ok(unsafe { app.as_ref() }
                .map(|a| a.processIdentifier())
                .filter(|pid| *pid > 0)
                .map(|pid| pid as u32)),
        };
        let _ = tx.send(outcome);
    });
    start(&handler);
    match rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(pid)) => Ok(LaunchResult { pid }),
        Ok(Err(msg)) => Err(Error::Platform(format!("opening {what} failed: {msg}"))),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::Timeout(format!(
            "opening {what} did not complete within {} s",
            OPEN_TIMEOUT.as_secs()
        ))),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::Platform(format!(
            "opening {what}: NSWorkspace dropped the completion handler"
        ))),
    }
}

fn configuration() -> Retained<NSWorkspaceOpenConfiguration> {
    let config = NSWorkspaceOpenConfiguration::configuration();
    config.setActivates(true);
    config
}

fn open_url(target: &str) -> Result<LaunchResult> {
    let url = NSURL::URLWithString(&NSString::from_str(target))
        .ok_or_else(|| Error::InvalidArgument(format!("'{target}' is not a valid URL")))?;
    let config = configuration();
    wait_open(target, |handler| {
        NSWorkspace::sharedWorkspace().openURL_configuration_completionHandler(
            &url,
            &config,
            Some(handler),
        )
    })
}

fn open_app(app: &Path, args: &[String]) -> Result<LaunchResult> {
    autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&app.to_string_lossy()));
        let config = configuration();
        if !args.is_empty() {
            let args: Vec<Retained<NSString>> =
                args.iter().map(|a| NSString::from_str(a)).collect();
            config.setArguments(&NSArray::from_retained_slice(&args));
        }
        wait_open(&app.display().to_string(), |handler| {
            NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
                &url,
                &config,
                Some(handler),
            )
        })
    })
}

fn open_document(path: &Path) -> Result<LaunchResult> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let config = configuration();
    wait_open(&path.display().to_string(), |handler| {
        NSWorkspace::sharedWorkspace().openURL_configuration_completionHandler(
            &url,
            &config,
            Some(handler),
        )
    })
}

fn spawn(exe: &Path, spec: &LaunchSpec) -> Result<LaunchResult> {
    let mut cmd = Command::new(exe);
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Own process group: not killed with ours, not tied to our terminal.
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Platform(format!("starting {}: {e}", exe.display())))?;
    let pid = child.id();
    // Reap the child when it exits so it does not linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(LaunchResult { pid: Some(pid) })
}
