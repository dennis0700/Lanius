//! System tray icon and menu management.
//!
//! Wraps the `tray_icon` crate to present Lanius's tray icon, its dropdown
//! menu (start/stop/restart server, show/hide window, quit, and a
//! non-interactive credit/usage line), and to translate raw menu-item click
//! events back into the app-level [`TrayCommand`] enum. `main.rs` owns the
//! [`Tray`] instance and polls `tray_icon`'s global event receivers on a
//! timer (menu clicks and icon double-clicks), while `controller.rs`
//! produces the label text (localized via `i18n`) and running/usage state
//! that get pushed into the tray via [`Tray::set_labels`],
//! [`Tray::set_running`], and [`Tray::set_usage`].
//!
//! The tray icon and its menu are native OS UI, so all mutation here happens
//! on whichever thread owns the platform event loop (driven by `main.rs`'s
//! timer callback), not from arbitrary background tasks.

use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// High-level actions a tray menu click can trigger, decoupled from the
/// underlying `tray_icon::menu::MenuId` used to identify menu items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    StartServer,
    StopServer,
    RestartServer,
    ShowWindow,
    HideWindow,
    Quit,
}

/// Localized label strings for every tray menu item, borrowed from the
/// caller for the duration of a single tray update (avoids allocating when
/// only some labels have actually changed).
pub struct TrayLabels<'a> {
    pub start: &'a str,
    pub stop: &'a str,
    pub restart: &'a str,
    pub show: &'a str,
    pub hide: &'a str,
    pub quit: &'a str,
}

/// Owns the platform tray icon and its menu items.
///
/// Keeps a handle to each [`MenuItem`] so their text/enabled state can be
/// updated in place after creation (the tray icon itself, `_icon`, is held
/// only to keep it alive — dropping it removes the icon from the system
/// tray).
pub struct Tray {
    _icon: TrayIcon,
    credit: MenuItem,
    start: MenuItem,
    stop: MenuItem,
    restart: MenuItem,
    show: MenuItem,
    hide: MenuItem,
    quit: MenuItem,
}

impl Tray {
    /// Builds the tray icon and its menu, using `labels` for the initial
    /// menu text.
    ///
    /// This creates real OS-level UI (a tray icon and menu) as a side
    /// effect, and can fail if the platform tray backend is unavailable
    /// (e.g. no system tray present) or if the bundled icon image fails to
    /// decode — callers (see `main.rs`) treat failure as non-fatal and keep
    /// retrying on a timer rather than crashing the app.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use crate::tray::{Tray, TrayLabels};
    ///
    /// let labels = TrayLabels { start: "Start", stop: "Stop", restart: "Restart", show: "Show", hide: "Hide", quit: "Quit" };
    /// let tray = Tray::new(&labels)?;
    /// ```
    pub fn new(labels: &TrayLabels<'_>) -> Result<Self, String> {
        let credit = MenuItem::new("Credit: --", false, None);
        let start = MenuItem::new(labels.start, true, None);
        let stop = MenuItem::new(labels.stop, false, None);
        let restart = MenuItem::new(labels.restart, false, None);
        let show = MenuItem::new(labels.show, true, None);
        let hide = MenuItem::new(labels.hide, true, None);
        let quit = MenuItem::new(labels.quit, true, None);

        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator();
        menu.append_items(&[
            &credit,
            &separator,
            &start,
            &stop,
            &restart,
            &PredefinedMenuItem::separator(),
            &show,
            &hide,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| format!("Failed to build tray menu: {e}"))?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Lanius")
            .with_icon(load_icon()?)
            .with_icon_as_template(true)
            .build()
            .map_err(|e| format!("Failed to create tray icon: {e}"))?;

        Ok(Self {
            _icon: icon,
            credit,
            start,
            stop,
            restart,
            show,
            hide,
            quit,
        })
    }

    /// Updates the text of every menu item (called after a language change).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use crate::tray::TrayLabels;
    ///
    /// let labels = TrayLabels { start: "启动", stop: "停止", restart: "重启", show: "显示", hide: "隐藏", quit: "退出" };
    /// tray.set_labels(&labels);
    /// ```
    pub fn set_labels(&self, labels: &TrayLabels<'_>) {
        self.start.set_text(labels.start);
        self.stop.set_text(labels.stop);
        self.restart.set_text(labels.restart);
        self.show.set_text(labels.show);
        self.hide.set_text(labels.hide);
        self.quit.set_text(labels.quit);
    }

    /// Synchronizes the enabled/disabled state of the start/stop/restart
    /// menu items with whether the embedded gateway is currently running,
    /// so users cannot start an already-running server or stop a stopped
    /// one from the tray menu.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Gateway just started: disable "Start", enable "Stop"/"Restart".
    /// tray.set_running(true);
    /// ```
    pub fn set_running(&self, running: bool) {
        self.start.set_enabled(!running);
        self.stop.set_enabled(running);
        self.restart.set_enabled(running);
    }

    /// Updates the non-interactive "Credit: ..." usage summary line shown
    /// at the top of the tray menu.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// tray.set_usage("Credit: 42%");
    /// ```
    pub fn set_usage(&self, text: &str) {
        self.credit.set_text(text);
    }

    /// Maps a raw menu-item id from a `tray_icon` menu event back to the
    /// [`TrayCommand`] it represents, or `None` if the id does not belong
    /// to one of this tray's known menu items (e.g. the credit line or a
    /// separator, neither of which is clickable/mapped).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
    ///     if let Some(command) = tray.command_for(&event.id) {
    ///         handle_tray_command(command);
    ///     }
    /// }
    /// ```
    pub fn command_for(&self, id: &MenuId) -> Option<TrayCommand> {
        if id == self.start.id() {
            Some(TrayCommand::StartServer)
        } else if id == self.stop.id() {
            Some(TrayCommand::StopServer)
        } else if id == self.restart.id() {
            Some(TrayCommand::RestartServer)
        } else if id == self.show.id() {
            Some(TrayCommand::ShowWindow)
        } else if id == self.hide.id() {
            Some(TrayCommand::HideWindow)
        } else if id == self.quit.id() {
            Some(TrayCommand::Quit)
        } else {
            None
        }
    }
}

/// Windows: loads the full-color app icon that `build.rs` embeds into the
/// exe as resource id 1. The macOS tray image is pure white (meant to be
/// recolored as a template image), which Windows doesn't do, so it would be
/// invisible on a light taskbar. Requesting 32x32 lets Windows pick the
/// closest size in the `.ico` and scale it for the current DPI.
#[cfg(target_os = "windows")]
fn load_icon() -> Result<tray_icon::Icon, String> {
    tray_icon::Icon::from_resource(1, Some((32, 32)))
        .map_err(|e| format!("Failed to load tray icon resource: {e}"))
}

/// Decodes the tray icon image bundled at compile time via
/// `include_bytes!`, converting it to the raw RGBA buffer `tray_icon`
/// expects. Rendered as a template image (`with_icon_as_template`) so macOS
/// can recolor it for light/dark menu bars.
#[cfg(not(target_os = "windows"))]
fn load_icon() -> Result<tray_icon::Icon, String> {
    const BYTES: &[u8] = include_bytes!("../assets/tray-icon.png");
    let image = image::load_from_memory(BYTES)
        .map_err(|e| format!("Failed to decode tray icon: {e}"))?
        .into_rgba8();
    let (width, height) = image.dimensions();
    tray_icon::Icon::from_rgba(image.into_raw(), width, height)
        .map_err(|e| format!("Failed to build tray icon: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_tray_icon_decodes() {
        let icon = load_icon();
        assert!(icon.is_ok(), "tray icon failed to decode: {icon:?}");
    }
}
