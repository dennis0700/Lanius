//! Application entry point: window/runtime bootstrap, tray event pump, and
//! Slint UI callback wiring.
//!
//! This is the top of the GUI's dependency graph — it owns the Tokio
//! runtime, the Slint `MainWindow`, and the single [`Controller`] instance,
//! and is responsible for connecting UI-generated events (button clicks,
//! form edits) and OS-level events (tray menu clicks, window close) to
//! `Controller` methods that perform the actual async work. `main()` itself
//! contains almost no business logic; it delegates to `controller.rs`
//! (start/stop/restart the embedded gateway, save config, etc.), `tray.rs`
//! (native tray icon/menu), `log_capture.rs` (installs the `tracing` layer
//! that feeds the GUI's log view), and, on macOS, `macos.rs` (Dock
//! visibility toggling to match window visibility).
//!
//! `slint::include_modules!()` pulls in the compiled Slint UI definitions
//! (e.g. `MainWindow`, `ConfigForm`, `Tr`, and other generated types
//! referenced throughout this crate) — this requires the Slint build
//! script to have run successfully, which in turn requires the platform's
//! GUI toolkit dependencies to be present at build time.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

mod api;
mod autostart;
mod config;
mod controller;
mod examples;
mod i18n;
mod log_capture;
mod logs;
mod process;
mod server;
mod tr_generated;
mod tray;
mod ui_state;
mod updater;

#[cfg(test)]
mod ui_tests;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, SharedString, Timer, TimerMode};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use controller::{Controller, copy_to_clipboard};

use i18n::Translations;
use log_capture::{CaptureLayer, LogBuffer};
use tray::Tray;

/// Preferred UI font per platform, chosen to render CJK text well on each
/// OS's default font stack (Slint's own default fonts have inconsistent
/// CJK glyph coverage).
fn ui_font() -> &'static str {
    #[cfg(target_os = "macos")]
    return "PingFang SC";
    #[cfg(target_os = "windows")]
    return "Microsoft YaHei UI";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return "Noto Sans CJK SC";
}

/// Preferred monospace font per platform, used for code snippets and log
/// text in the UI.
fn mono_font() -> &'static str {
    #[cfg(target_os = "macos")]
    return "Menlo";
    #[cfg(target_os = "windows")]
    return "Consolas";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return "monospace";
}

/// Application entry point.
///
/// Sets up logging (both the normal terminal formatter and the in-memory
/// [`CaptureLayer`] the GUI's log view reads from), builds a multi-threaded
/// Tokio runtime that the Slint UI thread hands async work off to, creates
/// the [`Controller`] and wires every Slint callback to it, starts the tray
/// icon/event-pump timer, then runs the Slint event loop until the app
/// quits. Blocks (via `runtime.block_on`) on [`Controller::shutdown`] after
/// the event loop exits, so the embedded gateway gets a chance to shut down
/// cleanly before the process exits.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let log_buffer = LogBuffer::new();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer().with_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            ),
        )
        .with(
            CaptureLayer::new(log_buffer.clone()).with_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            ),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();

    let ui = MainWindow::new()?;
    ui.set_is_mac(cfg!(target_os = "macos"));
    ui.set_app_version(api::app_version().into());

    let brand = ui.global::<Brand>();
    brand.set_ui_font(ui_font().into());
    brand.set_mono_font(mono_font().into());

    let controller = Controller::new(&ui, Translations::load(), log_buffer);

    wire_callbacks(&ui, &controller, &handle);

    {
        let weak = ui.as_weak();
        // Hiding the window on close (rather than letting Slint destroy it)
        // is what keeps Lanius running as a tray app after the user clicks
        // the window's close button; remember its position first so
        // `show_window` can restore it later.
        ui.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                LAST_WINDOW_POSITION.with(|cell| cell.set(Some(ui.window().position())));
            }
            #[cfg(target_os = "macos")]
            macos::set_dock_visible(false);
            slint::CloseRequestResponse::HideWindow
        });
    }

    {
        let controller = Arc::clone(&controller);
        // Bootstrap (loading config, auto-starting the server, scheduling
        // periodic usage checks) runs as a background async task so
        // the UI can render immediately rather than blocking startup on
        // config I/O or an initial health probe.
        handle.spawn(async move {
            controller.bootstrap().await;
        });
    }

    let tray_timer = Timer::default();
    {
        let controller = Arc::clone(&controller);
        let handle = handle.clone();
        let weak = ui.as_weak();
        let mut tray: Option<Tray> = None;
        let mut tray_failed = false;

        // The tray icon must be created/polled from this timer callback
        // (which runs on the Slint/main event loop thread) rather than from
        // an async task, since `tray_icon`'s platform backends are not
        // thread-safe. Creation is retried every tick until it succeeds
        // (some platforms need the event loop running first) unless it has
        // already failed once, in which case the app continues without a
        // tray icon rather than retrying forever.
        tray_timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
            if tray.is_none() && !tray_failed {
                let labels = controller.tray_labels();
                match Tray::new(&Controller::tray_labels_ref(&labels)) {
                    Ok(created) => tray = Some(created),
                    Err(e) => {
                        tracing::warn!("tray unavailable: {e}");
                        tray_failed = true;
                    }
                }
            }

            let Some(tray_icon) = tray.as_ref() else {
                return;
            };

            // Drain any pending updates the controller queued for the tray
            // (label text, running state, usage text, window show/hide/quit
            // requests) — `Controller` cannot touch the tray directly since
            // it lives on this thread, so it stages changes in
            // `Controller::tray` for this timer to apply.
            if let Ok(mut shared) = controller.tray.lock() {
                if let Some(labels) = shared.labels.take() {
                    tray_icon.set_labels(&Controller::tray_labels_ref(&labels));
                }
                if let Some(running) = shared.running.take() {
                    tray_icon.set_running(running);
                }
                if let Some(text) = shared.usage_text.take() {
                    tray_icon.set_usage(&text);
                }
                if std::mem::take(&mut shared.show_window) {
                    show_window(&weak);
                }
                if std::mem::take(&mut shared.hide_window) {
                    hide_window(&weak);
                }
                if std::mem::take(&mut shared.quit) {
                    slint::quit_event_loop().ok();
                }
            }

            // Drain tray menu click events (from `tray_icon`'s global
            // channel) and dispatch each to the controller as an async
            // task, since handling a tray command (e.g. starting the
            // server) does real async work.
            while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                if let Some(command) = tray_icon.command_for(&event.id) {
                    let controller = Arc::clone(&controller);
                    handle.spawn(async move {
                        controller.handle_tray_command(command).await;
                    });
                }
            }

            // Drain tray icon events, toggling window visibility on a
            // double-click (there is no dedicated single-click "toggle"
            // action on this platform's tray API, so double-click is used
            // as the show/hide gesture).
            while let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
                if matches!(event, tray_icon::TrayIconEvent::DoubleClick { .. }) {
                    match weak.upgrade() {
                        Some(ui) if ui.window().is_visible() => hide_window(&weak),
                        _ => show_window(&weak),
                    }
                }
            }
        });
    }

    ui.show()?;
    #[cfg(target_os = "macos")]
    macos::set_dock_visible(true);

    slint::run_event_loop_until_quit()?;

    runtime.block_on(async {
        controller.shutdown().await;
    });

    Ok(())
}

const WINDOW_LOGICAL_SIZE: (f32, f32) = (1180.0, 820.0);

thread_local! {
    /// Remembers the window's last on-screen position across hide/show
    /// cycles, since hiding the window (rather than destroying it) does not
    /// itself preserve a position Slint will restore automatically on the
    /// next `show()`.
    static LAST_WINDOW_POSITION: std::cell::Cell<Option<slint::PhysicalPosition>> =
        const { std::cell::Cell::new(None) };
}

/// Shows the main window at its standard logical size (not maximized),
/// restoring its previous on-screen position if one was recorded, and — on
/// macOS — makes the Dock icon visible again to match. Called from the
/// tray's "Show window" command and on a tray-icon double-click while the
/// window is hidden.
fn show_window(weak: &slint::Weak<MainWindow>) {
    if let Some(ui) = weak.upgrade() {
        #[cfg(target_os = "macos")]
        macos::set_dock_visible(true);
        let _ = ui.show();
        ui.window().set_maximized(false);
        let (width, height) = WINDOW_LOGICAL_SIZE;
        ui.window().set_size(slint::LogicalSize::new(width, height));
        if let Some(position) = LAST_WINDOW_POSITION.with(|cell| cell.get()) {
            ui.window().set_position(position);
        }
    }
}

/// Hides the main window (remembering its position for [`show_window`] to
/// restore later) and — on macOS — hides the Dock icon so Lanius continues
/// running quietly as a tray-only process.
fn hide_window(weak: &slint::Weak<MainWindow>) {
    if let Some(ui) = weak.upgrade() {
        LAST_WINDOW_POSITION.with(|cell| cell.set(Some(ui.window().position())));
        let _ = ui.hide();
        #[cfg(target_os = "macos")]
        macos::set_dock_visible(false);
    }
}

/// Connects every Slint UI callback (`ui.on_*`) declared on `MainWindow` to
/// its corresponding [`Controller`] method, spawning each as an async task
/// on the shared Tokio `handle` so UI callbacks (which run synchronously on
/// the Slint event loop thread) never block on async work.
///
/// The `spawn_task!` macro captures this common "clone the controller and
/// handle, spawn an async closure that clones them again" pattern for the
/// simple callbacks that take no UI-derived arguments; callbacks that need
/// to read something off the `MainWindow` first (e.g. the current form
/// values) are wired individually below instead, since they also need a
/// weak `MainWindow` handle.
fn wire_callbacks(ui: &MainWindow, controller: &Arc<Controller>, handle: &tokio::runtime::Handle) {
    macro_rules! spawn_task {
        ($body:expr_2021) => {{
            let controller = Arc::clone(controller);
            let handle = handle.clone();
            move || {
                let controller = Arc::clone(&controller);
                handle.spawn($body(controller));
            }
        }};
    }

    ui.on_start_server(spawn_task!(|c: Arc<Controller>| async move {
        c.start_server().await;
    }));
    ui.on_stop_server(spawn_task!(|c: Arc<Controller>| async move {
        c.stop_server().await;
    }));
    ui.on_restart_server(spawn_task!(|c: Arc<Controller>| async move {
        c.restart_server().await;
    }));
    ui.on_refresh_usage(spawn_task!(|c: Arc<Controller>| async move {
        c.refresh_usage(false).await;
        c.refresh_models().await;
    }));
    ui.on_clear_logs(spawn_task!(|c: Arc<Controller>| async move {
        c.clear_logs().await;
    }));
    ui.on_check_for_updates(spawn_task!(|c: Arc<Controller>| async move {
        c.check_for_updates().await;
    }));
    ui.on_install_update(spawn_task!(|c: Arc<Controller>| async move {
        c.install_update().await;
    }));
    ui.on_open_release_page(spawn_task!(|c: Arc<Controller>| async move {
        c.open_release_page().await;
    }));

    {
        let controller = Arc::clone(controller);
        let handle = handle.clone();
        let weak = ui.as_weak();
        ui.on_form_changed(move || {
            let Some(ui) = weak.upgrade() else { return };
            let form = ui.get_form();
            let controller = Arc::clone(&controller);
            handle.spawn(async move {
                controller.form_changed(form).await;
            });
        });
    }

    {
        let controller = Arc::clone(controller);
        let handle = handle.clone();
        let weak = ui.as_weak();
        ui.on_save_config(move || {
            let Some(ui) = weak.upgrade() else { return };
            let form = ui.get_form();
            let controller = Arc::clone(&controller);
            handle.spawn(async move {
                controller.save_config(form).await;
            });
        });
    }

    {
        let controller = Arc::clone(controller);
        let handle = handle.clone();
        let weak = ui.as_weak();
        ui.on_generate_key(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut form = ui.get_form();
            form.proxy_api_key = config::generate_api_key().into();
            ui.set_form(form.clone());
            let controller = Arc::clone(&controller);
            handle.spawn(async move {
                controller.form_changed(form).await;
            });
        });
    }

    {
        let controller = Arc::clone(controller);
        let handle = handle.clone();
        let weak = ui.as_weak();
        ui.on_example_changed(move |flavor, snippet| {
            let _ = &weak;
            let controller = Arc::clone(&controller);
            handle.spawn(async move {
                controller.set_example_selection(flavor, snippet).await;
            });
        });
    }

    {
        let controller = Arc::clone(controller);
        let handle = handle.clone();
        ui.on_select_language(move |index| {
            let controller = Arc::clone(&controller);
            handle.spawn(async move {
                controller.set_language(index).await;
            });
        });
    }

    {
        let controller = Arc::clone(controller);
        let handle = handle.clone();
        let weak = ui.as_weak();
        ui.on_restart_after_save(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_restart_prompt(false);
                ui.set_save_success(false);
            }
            let controller = Arc::clone(&controller);
            handle.spawn(async move {
                controller.restart_server().await;
            });
        });
    }

    ui.on_copy_text(|text| copy_to_clipboard(text.as_str()));

    {
        let weak = ui.as_weak();
        let reset_timer = Timer::default();
        // Copying the proxy API key also flips a "copied!" flag for 2
        // seconds purely for UI feedback; the timer is owned by this
        // closure so it lives as long as the callback itself.
        ui.on_copy_key(move || {
            let Some(ui) = weak.upgrade() else { return };
            copy_to_clipboard(ui.get_form().proxy_api_key.as_str());
            ui.set_key_copied(true);
            let weak = weak.clone();
            reset_timer.start(TimerMode::SingleShot, Duration::from_secs(2), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_key_copied(false);
                }
            });
        });
    }

    {
        let weak = ui.as_weak();
        let reset_timer = Timer::default();
        // Copying a code example also flips a "copied!" flag for 2 seconds
        // via a one-shot timer, purely for UI feedback; the timer is owned
        // by this closure so it lives as long as the callback itself.
        ui.on_copy_code(move || {
            let Some(ui) = weak.upgrade() else { return };
            copy_to_clipboard(ui.get_example_code().as_str());
            ui.set_example_copied(true);
            let weak = weak.clone();
            reset_timer.start(TimerMode::SingleShot, Duration::from_secs(2), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_example_copied(false);
                }
            });
        });
    }

    {
        let controller = Arc::clone(controller);
        let handle = handle.clone();
        ui.on_export_logs(move || {
            let Some(path) = rfd::FileDialog::new()
                .set_file_name(format!("lanius-logs-{}.txt", unix_seconds()))
                .add_filter("Text Files", &["txt"])
                .save_file()
            else {
                return;
            };
            let controller = Arc::clone(&controller);
            handle.spawn(async move {
                let text = controller.export_text().await;
                if let Err(e) = tokio::fs::write(&path, text).await {
                    tracing::warn!("failed to export logs to {}: {e}", path.display());
                }
            });
        });
    }
}

/// Returns the current Unix time in whole seconds, used to build a unique
/// default filename when exporting logs. Falls back to `0` if the system
/// clock is somehow set before the Unix epoch, rather than panicking.
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

const _: Option<SharedString> = None;
