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

#[cfg(not(windows))]
compile_error!(
    "deskpilot has no adapter for this operating system yet; implement deskpilot_core::platform::Platform for it"
);
