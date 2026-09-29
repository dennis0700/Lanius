//! macOS-specific window/Dock integration helpers.
//!
//! Lanius GUI runs as a tray application: most of the time the main window is
//! hidden and only the system tray icon is visible. On macOS, hiding a window
//! does not by itself hide the application's Dock icon, so `main.rs` calls
//! into this module whenever the window is shown or hidden to toggle the
//! process's Dock/menu-bar presence accordingly (an "Accessory" app has no
//! Dock icon and cannot become key/front automatically; a "Regular" app
//! behaves like a normal Mac application).
//!
//! This entire module is compiled only on macOS (`#![cfg(target_os =
//! "macos")]`) and must be called from the main thread, since AppKit APIs are
//! not thread-safe.

#![cfg(target_os = "macos")]

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;

/// Shows or hides this application's Dock icon (and menu bar presence).
///
/// Pass `true` when the main window becomes visible so the app behaves like
/// a normal foreground application (Dock icon, can be activated); pass
/// `false` when the window is hidden so the app keeps running quietly as a
/// tray-only "accessory" process.
///
/// Side effects / preconditions:
/// - Must be called from the main thread. If no `MainThreadMarker` can be
///   obtained (i.e. this is not the main thread), the function silently does
///   nothing rather than panicking.
/// - When making the app visible, this also brings it to the front via
///   `activateIgnoringOtherApps` (a deprecated but still functional AppKit
///   API, hence the `#[allow(deprecated)]`).
///
/// # Examples
///
/// ```ignore
/// // On the main thread, after hiding the main window to the tray:
/// crate::macos::set_dock_visible(false);
/// ```
pub fn set_dock_visible(visible: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    app.setActivationPolicy(policy);
    if visible {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}
