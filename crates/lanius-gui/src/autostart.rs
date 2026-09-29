//! "Launch at login" (auto-start) integration.
//!
//! Wraps the `auto_launch` crate to register or unregister Lanius as a
//! login item with the operating system, driven by the `auto_launch` field
//! in [`crate::config::AppConfig`]. `controller.rs` calls [`apply`] during
//! bootstrap and whenever the user changes the setting and saves the
//! configuration.
//!
//! Platform behavior differs:
//! - On macOS, the login item is registered via AppleScript automation
//!   against the `.app` bundle (not the raw executable inside it), because
//!   login items must point at an application bundle to appear correctly in
//!   System Settings.
//! - On Windows, a per-user `HKCU\...\Run` registry entry points at the
//!   raw executable (no admin rights needed).
//! - On other platforms, the raw executable path is used directly.

use auto_launch::AutoLaunch;
#[cfg(target_os = "macos")]
use auto_launch::MacOSLaunchMode;
#[cfg(target_os = "windows")]
use auto_launch::WindowsEnableMode;

const APP_NAME: &str = "Lanius";

/// Resolves the path that should be registered as the login-item target.
///
/// On macOS this walks up from the current executable's path to find the
/// enclosing `.app` bundle (since `current_exe()` returns a path deep inside
/// `Lanius.app/Contents/MacOS/lanius-gui`), falling back to the raw
/// executable path if no `.app` ancestor is found. On other platforms the
/// executable path is used as-is.
///
/// Returns an error if the current executable's path cannot be determined
/// or is not valid UTF-8 (required by the `auto_launch` API).
fn launch_target() -> Result<String, String> {
    let exe_path =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;

    #[cfg(target_os = "macos")]
    let app_path = {
        let mut found = None;
        let mut p = exe_path.as_path();
        while let Some(parent) = p.parent() {
            if p.extension().and_then(|e| e.to_str()) == Some("app") {
                found = Some(p.to_path_buf());
                break;
            }
            p = parent;
        }
        found.unwrap_or(exe_path)
    };
    #[cfg(not(target_os = "macos"))]
    let app_path = exe_path;

    app_path
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "App path is not valid UTF-8".to_string())
}

/// Enables or disables launching Lanius automatically at user login.
///
/// This performs a platform-specific, synchronous write to OS-level login
/// item state (e.g. via AppleScript on macOS, or the registry/autostart
/// folder on other platforms through the `auto_launch` crate) — it does not
/// touch Lanius's own config file. Call sites are responsible for persisting
/// the `auto_launch` preference separately via [`crate::config`].
///
/// # Examples
///
/// ```ignore
/// // Register Lanius as a login item, then remove it again.
/// crate::autostart::apply(true)?;
/// crate::autostart::apply(false)?;
/// ```
pub fn apply(enabled: bool) -> Result<(), String> {
    let app_str = launch_target()?;

    #[cfg(target_os = "macos")]
    let auto = AutoLaunch::new(
        APP_NAME,
        &app_str,
        MacOSLaunchMode::AppleScript,
        &[] as &[&str],
        &[] as &[&str],
        "",
    );
    // Per-user `HKCU\...\Run` entry: never needs admin rights, and matches
    // the per-user install location the Windows build targets.
    #[cfg(target_os = "windows")]
    let auto = AutoLaunch::new(
        APP_NAME,
        &app_str,
        WindowsEnableMode::CurrentUser,
        &[] as &[&str],
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let auto = AutoLaunch::new(APP_NAME, &app_str, &[] as &[&str]);

    if enabled {
        auto.enable()
            .map_err(|e| format!("Failed to enable auto-launch: {}", e))
    } else {
        auto.disable()
            .map_err(|e| format!("Failed to disable auto-launch: {}", e))
    }
}
