//! TCC privacy permissions: live status and the start-up window that asks the
//! user to grant the missing ones.
//!
//! The window lists every permission with a button that triggers the system
//! request and one that opens the right System Settings pane, re-checks the
//! state every second, turns each row green once granted and closes itself
//! when nothing is missing. It never appears while everything is granted, so
//! it only comes back if the user revokes a permission.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::mpsc;

use block2::RcBlock;
use deskpilot_core::platform::PermissionStatus;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColor, NSControl, NSFloatingWindowLevel, NSFont,
    NSFontWeightRegular, NSImage, NSImageSymbolConfiguration, NSImageView, NSLayoutAttribute,
    NSStackView, NSTextField, NSUserInterfaceLayoutOrientation, NSView, NSWindow,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_application_services::{AXIsProcessTrusted, AXIsProcessTrustedWithOptions};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
use objc2_core_graphics::{
    CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess, CGSessionCopyCurrentDictionary,
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID,
};
use objc2_foundation::{NSArray, NSEdgeInsets, NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL};

use crate::cf;
use crate::main_thread::on_main;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Permission {
    Accessibility,
    ScreenRecording,
}

const ALL: [Permission; 2] = [Permission::Accessibility, Permission::ScreenRecording];

impl Permission {
    fn id(self) -> &'static str {
        match self {
            Permission::Accessibility => "accessibility",
            Permission::ScreenRecording => "screen_recording",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Permission::Accessibility => "Accessibility",
            Permission::ScreenRecording => "Screen & System Audio Recording",
        }
    }

    fn needed_for(self) -> &'static str {
        match self {
            Permission::Accessibility => {
                "reading and acting on app controls, keyboard and mouse input, window management"
            }
            Permission::ScreenRecording => "screenshots, OCR, pixel colours and window titles",
        }
    }

    fn settings_url(self) -> &'static str {
        match self {
            Permission::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Permission::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
        }
    }

    fn granted(self) -> bool {
        match self {
            Permission::Accessibility => unsafe { AXIsProcessTrusted() },
            Permission::ScreenRecording => screen_recording_granted(),
        }
    }

    /// Shows the system request (only the first time macOS asks; afterwards the
    /// user has to use System Settings).
    fn request(self) {
        match self {
            Permission::Accessibility => {
                let key: &CFString = unsafe { objc2_application_services::kAXTrustedCheckOptionPrompt };
                let options = CFDictionary::<CFString, CFBoolean>::from_slices(
                    &[key],
                    &[CFBoolean::new(true)],
                );
                unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) };
            }
            Permission::ScreenRecording => {
                CGRequestScreenCaptureAccess();
            }
        }
    }

    fn from_tag(tag: isize) -> Option<Self> {
        ALL.get(usize::try_from(tag).ok()?).copied()
    }
}

/// Live Screen Recording state. `CGPreflightScreenCaptureAccess` keeps
/// answering `false` for the lifetime of the process once it did, so a grant
/// made while deskpilot runs is detected through the window list: without the
/// permission, titles of other processes' windows are hidden.
pub(crate) fn screen_recording_granted() -> bool {
    CGPreflightScreenCaptureAccess() || window_titles_visible()
}

/// `true` when `CGPreflightScreenCaptureAccess` is stale: the permission was
/// granted after this process started, and capture APIs may keep failing until
/// it restarts.
pub(crate) fn screen_recording_needs_restart() -> bool {
    !CGPreflightScreenCaptureAccess() && window_titles_visible()
}

fn window_titles_visible() -> bool {
    let me = std::process::id() as i64;
    let Some(list) = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return false;
    };
    cf::array(&list).iter().any(|w| {
        let Some(dict) = w.downcast_ref::<CFDictionary>() else {
            return false;
        };
        let pid = cf::dict_get(dict, "kCGWindowOwnerPID")
            .and_then(|v| cf::number(&v))
            .map(|n| n as i64);
        let owner = cf::dict_get(dict, "kCGWindowOwnerName").and_then(|v| cf::string(&v));
        // The Dock and the window server expose titles regardless of the permission.
        if pid == Some(me) || matches!(owner.as_deref(), Some("Dock" | "Window Server")) {
            return false;
        }
        cf::dict_get(dict, "kCGWindowName")
            .and_then(|v| cf::string(&v))
            .is_some_and(|name| !name.is_empty())
    })
}

pub fn status() -> Vec<PermissionStatus> {
    ALL.iter()
        .map(|p| PermissionStatus {
            id: p.id(),
            granted: p.granted(),
            needed_for: p.needed_for(),
        })
        .collect()
}

/// Whether a window can be shown at all (logged-in GUI session, not ssh).
fn has_gui_session() -> bool {
    CGSessionCopyCurrentDictionary().is_some()
}

/// What `gate` did, so the caller can restart when a grant only applies to a
/// fresh process.
#[derive(Debug, PartialEq, Eq)]
pub enum GateOutcome {
    /// Nothing was missing; no window.
    AllGranted,
    /// The window was shown and the call returned without waiting.
    Shown,
    /// The window was shown and the user granted everything.
    GrantedWhileWaiting,
    /// The user closed the window with permissions still missing.
    Dismissed,
    /// No GUI session to show a window in.
    NoSession,
}

/// Shows the permission window when something is missing. With `wait`, blocks
/// until everything is granted or the user closes the window. Must not be
/// called on the main thread (the window lives on the main run loop).
pub fn gate(wait: bool) -> GateOutcome {
    if ALL.iter().all(|p| p.granted()) {
        return GateOutcome::AllGranted;
    }
    if !has_gui_session() {
        return GateOutcome::NoSession;
    }
    let (tx, rx) = mpsc::channel::<bool>();
    on_main(move |mtm| show(mtm, tx));
    if !wait {
        return GateOutcome::Shown;
    }
    match rx.recv() {
        Ok(true) => GateOutcome::GrantedWhileWaiting,
        _ => GateOutcome::Dismissed,
    }
}

// ---------------------------------------------------------------------------
// Window (main thread only)
// ---------------------------------------------------------------------------

struct Row {
    permission: Permission,
    icon: Retained<NSImageView>,
    request: Retained<NSButton>,
    open: Retained<NSButton>,
}

struct Gate {
    window: Retained<NSWindow>,
    rows: Vec<Row>,
    timer: Retained<NSTimer>,
    /// `true` = everything granted, `false` = dismissed.
    done: mpsc::Sender<bool>,
    _target: Retained<ButtonTarget>,
}

thread_local! {
    static GATE: RefCell<Option<Gate>> = const { RefCell::new(None) };
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct ButtonTarget;

    impl ButtonTarget {
        #[unsafe(method(request:))]
        fn request(&self, sender: &NSControl) {
            if let Some(p) = Permission::from_tag(sender.tag()) {
                p.request();
                refresh();
            }
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, sender: &NSControl) {
            if let Some(p) = Permission::from_tag(sender.tag()) {
                if let Some(url) = NSURL::URLWithString(&NSString::from_str(p.settings_url())) {
                    NSWorkspace::sharedWorkspace().openURL(&url);
                }
            }
        }
    }

    unsafe impl NSObjectProtocol for ButtonTarget {}
);

impl ButtonTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    NSTextField::labelWithString(&NSString::from_str(text), mtm)
}

fn wrapping(text: &str, width: f64, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let l = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    l.setPreferredMaxLayoutWidth(width);
    l.setSelectable(false);
    l
}

fn stack(
    views: &[&NSView],
    orientation: NSUserInterfaceLayoutOrientation,
    spacing: f64,
    mtm: MainThreadMarker,
) -> Retained<NSStackView> {
    let s = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm);
    s.setOrientation(orientation);
    s.setSpacing(spacing);
    s.setAlignment(if orientation == NSUserInterfaceLayoutOrientation::Vertical {
        NSLayoutAttribute::Leading
    } else {
        NSLayoutAttribute::CenterY
    });
    s
}

fn button(
    title: &str,
    action: objc2::runtime::Sel,
    tag: usize,
    target: &ButtonTarget,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action),
            mtm,
        )
    };
    b.setTag(tag as isize);
    b
}

fn show(mtm: MainThreadMarker, done: mpsc::Sender<bool>) {
    // A previous window is still up (MCP server restarted the check): reuse it.
    if GATE.with(|g| g.borrow().is_some()) {
        return;
    }
    let app = NSApplication::sharedApplication(mtm);
    let target = ButtonTarget::new(mtm);
    const WIDTH: f64 = 520.0;

    let heading = label("deskpilot needs your permission", mtm);
    heading.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));
    let intro = wrapping(
        "macOS asks before an app can read other apps' controls, send keyboard and mouse input or capture the screen. Grant each permission below; this window checks every second and closes by itself once everything is granted.",
        WIDTH,
        mtm,
    );

    let mut content: Vec<Retained<NSView>> = vec![
        Retained::into_super(Retained::into_super(heading)),
        Retained::into_super(Retained::into_super(intro)),
    ];
    let mut rows = Vec::new();
    for (i, p) in ALL.iter().copied().enumerate() {
        let icon = NSImageView::new(mtm);
        icon.setSymbolConfiguration(Some(&NSImageSymbolConfiguration::configurationWithPointSize_weight(
            22.0,
            unsafe { NSFontWeightRegular },
        )));
        let title = label(p.title(), mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        let detail = wrapping(&format!("Needed for {}.", p.needed_for()), 260.0, mtm);
        detail.setTextColor(Some(&NSColor::secondaryLabelColor()));
        detail.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        let text = stack(
            &[&title, &detail],
            NSUserInterfaceLayoutOrientation::Vertical,
            2.0,
            mtm,
        );
        let request = button("Request", sel!(request:), i, &target, mtm);
        let open = button("Open Settings", sel!(openSettings:), i, &target, mtm);
        let row = stack(
            &[&icon, &text, &request, &open],
            NSUserInterfaceLayoutOrientation::Horizontal,
            10.0,
            mtm,
        );
        content.push(Retained::into_super(row));
        rows.push(Row {
            permission: p,
            icon,
            request,
            open,
        });
    }
    let footer = wrapping(
        "If macOS offers to “Quit & Reopen” deskpilot after you enable Screen Recording, choose “Later”: deskpilot picks the permission up by itself. Closing this window leaves the missing permissions off; the tools that need them report it.",
        WIDTH,
        mtm,
    );
    footer.setTextColor(Some(&NSColor::secondaryLabelColor()));
    footer.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    content.push(Retained::into_super(Retained::into_super(footer)));

    let refs: Vec<&NSView> = content.iter().map(|v| &**v).collect();
    let body = stack(&refs, NSUserInterfaceLayoutOrientation::Vertical, 14.0, mtm);
    body.setEdgeInsets(NSEdgeInsets {
        top: 20.0,
        left: 20.0,
        bottom: 20.0,
        right: 20.0,
    });

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH + 40.0, 300.0)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // The window is owned by `Gate`; AppKit must not free it on close.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("deskpilot — permissions"));
    window.setContentView(Some(&body));
    window.setContentSize(body.fittingSize());
    window.setLevel(NSFloatingWindowLevel);
    window.center();

    let tick = RcBlock::new(|_: NonNull<NSTimer>| refresh());
    let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &tick) };

    GATE.with(|g| {
        *g.borrow_mut() = Some(Gate {
            window: window.clone(),
            rows,
            timer,
            done,
            _target: target,
        })
    });
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    // Only after the window is on screen: `refresh` reads "not visible" as "closed by the user".
    refresh();
}

/// Re-checks every permission, updates the rows and closes the window when
/// everything is granted (or forgets it once the user closed it).
fn refresh() {
    let finished = GATE.with(|g| {
        let g = g.borrow();
        let gate = g.as_ref()?;
        if !gate.window.isVisible() {
            return Some(false);
        }
        let mut all = true;
        for row in &gate.rows {
            let granted = row.permission.granted();
            all &= granted;
            let (symbol, color) = if granted {
                ("checkmark.circle.fill", NSColor::systemGreenColor())
            } else {
                ("exclamationmark.circle.fill", NSColor::systemOrangeColor())
            };
            row.icon.setImage(
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(symbol),
                    Some(&NSString::from_str(if granted { "granted" } else { "missing" })),
                )
                .as_deref(),
            );
            row.icon.setContentTintColor(Some(&color));
            row.request.setHidden(granted);
            row.open.setHidden(granted);
        }
        all.then_some(true)
    });
    if let Some(granted) = finished {
        GATE.with(|g| {
            if let Some(gate) = g.borrow_mut().take() {
                gate.timer.invalidate();
                gate.window.close();
                let _ = gate.done.send(granted);
            }
        });
    }
}
