use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    match deskpilot_windows::WindowsPlatform::new() {
        Ok(platform) => deskpilot_core::cli::run(Box::new(platform)),
        Err(e) => {
            eprintln!("deskpilot: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    // Become our own privacy (TCC) identity instead of the launching terminal or MCP client.
    deskpilot_macos::disclaim_responsibility();
    // The main thread runs AppKit (permission window); the program runs beside it.
    deskpilot_macos::run(|| match deskpilot_macos::MacPlatform::new() {
        Ok(platform) => deskpilot_core::cli::run(Box::new(platform)),
        Err(e) => {
            eprintln!("deskpilot: {e}");
            ExitCode::FAILURE
        }
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!(
    "deskpilot has no adapter for this operating system yet; implement deskpilot_core::platform::Platform for it"
);
