//! Windows-specific integration: single-instance enforcement, OS locale
//! lookup, and opening URLs without a console flash.
//!
//! Like `macos.rs`, this entire module is compiled only on its own platform
//! (`#![cfg(target_os = "windows")]` below and the matching `mod` in
//! `main.rs`), so nothing here affects the macOS or Linux builds.
//!
//! Single instance: macOS itself refocuses a running `.app` instead of
//! starting a second copy, but on Windows double-clicking the exe twice
//! really does start two processes — and the second one would then find the
//! gateway port taken, recognise the occupier as a Lanius process, and kill
//! the first instance (see `process::terminate_process`). A named mutex
//! prevents that; the second instance signals a named event instead so the
//! first can bring its window to the front, then exits.

#![cfg(target_os = "windows")]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

// `Local\` scopes both objects to the current login session, so two users
// signed in at once (fast user switching, RDP) each get their own instance.
const MUTEX_NAME: &str = r"Local\com.dennis0700.lanius.instance";
const ACTIVATE_EVENT_NAME: &str = r"Local\com.dennis0700.lanius.activate";

/// Encodes `s` as a NUL-terminated UTF-16 string for Win32 `*W` APIs.
fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Proof that this process is the primary instance. Holding it keeps the
/// named mutex alive; Windows releases it automatically when the process
/// exits, including on a crash, so a stale lock can never block startup.
pub struct InstanceGuard {
    mutex: HANDLE,
}

// SAFETY: the guard only holds a kernel handle and never touches it except
// in `Drop`; kernel handles are process-wide, not thread-affine.
unsafe impl Send for InstanceGuard {}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if !self.mutex.is_null() {
            unsafe { CloseHandle(self.mutex) };
        }
    }
}

/// Tries to become the only running instance in this login session.
///
/// Returns `Some(guard)` if this is the first instance (hold the guard for
/// the lifetime of the app). Returns `None` if another instance already
/// holds the lock, after signalling it to show its window — the caller
/// should exit immediately.
///
/// If the mutex can't be created at all (should not happen in practice), it
/// fails open and lets the app start rather than refusing to launch.
///
/// # Examples
///
/// ```ignore
/// let Some(_instance_guard) = crate::windows::acquire_single_instance() else {
///     return Ok(()); // another instance was asked to show its window
/// };
/// ```
pub fn acquire_single_instance() -> Option<InstanceGuard> {
    let name = wide(MUTEX_NAME);
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

    if mutex.is_null() {
        tracing::warn!("single-instance mutex unavailable; continuing without it");
        return Some(InstanceGuard {
            mutex: std::ptr::null_mut(),
        });
    }

    if already_running {
        unsafe { CloseHandle(mutex) };
        signal_existing_instance();
        return None;
    }

    Some(InstanceGuard { mutex })
}

/// Asks the already-running instance to show its window, via the
/// auto-reset event created by [`listen_for_activation`].
fn signal_existing_instance() {
    let name = wide(ACTIVATE_EVENT_NAME);
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if event.is_null() {
        return;
    }
    unsafe {
        SetEvent(event);
        CloseHandle(event);
    }
}

/// Spawns a background thread that calls `on_activate` every time a second
/// launch of the app signals [`signal_existing_instance`]. The thread (and
/// the event handle it owns) lives until the process exits.
///
/// # Examples
///
/// ```ignore
/// crate::windows::listen_for_activation(|| {
///     tracing::info!("second launch detected; showing the main window");
/// });
/// ```
pub fn listen_for_activation(on_activate: impl Fn() + Send + 'static) {
    let name = wide(ACTIVATE_EVENT_NAME);
    // Auto-reset (bManualReset = FALSE), initially non-signalled.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if event.is_null() {
        tracing::warn!("activation event unavailable; a second launch won't refocus this window");
        return;
    }
    // Raw handles aren't `Send`; pass the value across as an integer.
    let event = event as usize;
    std::thread::Builder::new()
        .name("lanius-activate".into())
        .spawn(move || {
            let event = event as HANDLE;
            loop {
                if unsafe { WaitForSingleObject(event, INFINITE) } != 0 {
                    // WAIT_FAILED/WAIT_ABANDONED: stop rather than spin.
                    break;
                }
                on_activate();
            }
        })
        .ok();
}

/// Returns the user's default locale name (e.g. `zh-CN`, `en-US`), or
/// `None` if the lookup fails. Windows apps usually don't see `LANG`/
/// `LC_*` environment variables, so `i18n` asks the OS directly instead.
///
/// # Examples
///
/// ```ignore
/// if let Some(locale) = crate::windows::user_locale() {
///     tracing::debug!("system locale: {locale}"); // e.g. "zh-CN"
/// }
/// ```
pub fn user_locale() -> Option<String> {
    // LOCALE_NAME_MAX_LENGTH is 85 (including the terminating NUL).
    let mut buf = [0u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 1 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..(len - 1) as usize]))
}

/// Opens `url` with the user's default handler via `ShellExecuteW`.
///
/// Preferred over `cmd /c start`, which re-parses its arguments (so `&` in
/// a URL would split the command) and briefly flashes a console window when
/// run from the GUI process. Only `http(s)` URLs are accepted, so this can
/// never be coerced into launching a local executable.
///
/// # Examples
///
/// ```ignore
/// crate::windows::open_url("https://github.com/")?;
/// assert!(crate::windows::open_url("C:\\Windows\\notepad.exe").is_err());
/// ```
pub fn open_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("refusing to open non-http(s) URL: {url}"));
    }
    let verb = wide("open");
    let file = wide(url);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value > 32 on success.
    if result as usize > 32 {
        Ok(())
    } else {
        Err(format!(
            "ShellExecuteW failed with code {}",
            result as usize
        ))
    }
}
