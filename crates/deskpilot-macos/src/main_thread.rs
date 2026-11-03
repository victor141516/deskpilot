//! The main thread belongs to AppKit (the permission window), so the program
//! itself runs on a background thread (`run`) and main-thread-only APIs
//! (NSScreen, windows, keyboard layouts) go through `on_main`.

use std::io::Write;
use std::process::ExitCode;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

/// Runs `f` on the main thread and returns its result (inline when already there).
pub(crate) fn on_main<R: Send>(f: impl FnOnce(MainThreadMarker) -> R + Send) -> R {
    dispatch2::run_on_main(f)
}

/// Runs `program` on a worker thread while the main thread serves the AppKit
/// run loop; exits the process with its code. Call from `main`.
pub fn run(program: impl FnOnce() -> ExitCode + Send + 'static) -> ! {
    let mtm = MainThreadMarker::new().expect("deskpilot_macos::run must be called from main");
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon or menu bar; windows can still be shown and focused.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    std::thread::Builder::new()
        .name("deskpilot".into())
        .spawn(move || {
            // A panic would otherwise leave the main thread serving the run loop forever.
            let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(program))
                .unwrap_or(ExitCode::from(101));
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            // `ExitCode` is opaque on stable Rust; compare against every value `cli::run` uses.
            let raw = (0..=u8::MAX)
                .find(|c| ExitCode::from(*c) == code)
                .map_or(1, i32::from);
            std::process::exit(raw);
        })
        .expect("spawn program thread");
    app.run();
    // `run` only returns if something stopped the application.
    std::process::exit(0);
}
