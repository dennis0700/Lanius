//! Core business-logic controller: the single object that wires UI state,
//! the embedded gateway, i18n, the tray, and background maintenance tasks
//! together.
//!
//! [`Controller`] is the hub of the GUI's architecture. `main.rs` creates
//! exactly one instance (wrapped in `Arc`, since it's shared across every
//! Slint callback and every spawned background task) and forwards every
//! Slint UI callback and tray command to a corresponding async method here.
//! From this module's perspective:
//! - `server.rs`'s [`ServerManager`] is the embedded gateway's lifecycle,
//!   owned behind `self.server: Mutex<ServerManager>`.
//! - `config.rs` is the persisted settings this controller reads/writes.
//! - `api.rs` is how it talks to the *running* gateway's own HTTP API
//!   (usage, models, health) once started.
//! - `process.rs` is how it detects/resolves port conflicts before
//!   (re)starting the gateway.
//! - `i18n.rs`/`tr_generated.rs` supply the translated strings pushed both
//!   to the Slint `Tr` global and to the tray menu.
//! - `tray.rs` is driven indirectly: this module never touches a `Tray`
//!   directly (it doesn't live on this thread) and instead stages updates
//!   into `self.tray: Arc<StdMutex<TrayShared>>`, which `main.rs`'s tray
//!   timer callback drains and applies.
//! - `ui_state.rs` converts between this module's domain types
//!   ([`AppConfig`], [`UsageSummary`], processed log lines) and the Slint
//!   UI's generated struct types.
//!
//! Every public async method here is meant to be spawned as its own Tokio
//! task from a Slint callback (see `main.rs::wire_callbacks`) — none of them
//! assume they run on any particular thread, but all UI mutation goes
//! through [`Controller::with_ui`], which marshals the closure onto the
//! Slint UI thread via `upgrade_in_event_loop`.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use slint::{ComponentHandle, SharedString, VecModel, Weak};
use tokio::sync::Mutex;

use crate::api::{self, UsageSummary};
use crate::config::{self, AppConfig};
use crate::examples::{self, ApiFlavor, Snippet};
use crate::i18n::Translations;
use crate::log_capture::LogBuffer;
use crate::logs;
use crate::process;
use crate::server::ServerManager;
use crate::tray::{TrayCommand, TrayLabels};
use crate::ui_state::{self, usage_placeholder};
use crate::updater::{self, AvailableUpdate};
use crate::{ConfigForm, MainWindow, ModelRow, Tr};

/// How often the background task in [`Controller::bootstrap`] re-checks
/// GitHub for a newer release while `auto_check_updates` is enabled.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long [`Controller::bootstrap`] waits before the very first automatic
/// update check, so it doesn't compete with startup work (config load,
/// gateway auto-start, initial usage/model fetch) for the network/CPU.
const UPDATE_CHECK_STARTUP_DELAY: Duration = Duration::from_secs(20);

/// Tray updates staged by [`Controller`] for `main.rs`'s tray timer to
/// apply, since the actual `tray::Tray` handle lives on the main/event-loop
/// thread and cannot be touched directly from here. Each `Option`/`bool`
/// field is a one-shot request: the timer callback takes the value (via
/// `Option::take`/`std::mem::take`) when it applies it, leaving it cleared
/// until the next update.
#[derive(Default)]
pub struct TrayShared {
    pub usage_text: Option<String>,
    pub running: Option<bool>,
    pub labels: Option<[String; 6]>,
    pub show_window: bool,
    pub hide_window: bool,
    pub quit: bool,
}

/// Which server operation is currently in flight, used to suppress
/// [`Controller::apply_running`] updates from the periodic status-poll loop
/// while a manual start/stop is already driving that same UI state, to
/// avoid the two racing and flickering the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Start,
    Stop,
}

/// All mutable controller state that isn't otherwise owned by a
/// sub-component (`ServerManager`, `Translations`, the tray staging area).
/// Guarded by a single async `Mutex` since it's read and written from many
/// different async methods, often across `.await` points.
#[derive(Default)]
struct State {
    config: AppConfig,
    config_writable: bool,
    form_published: bool,
    pending: Option<Pending>,
    api_flavor: i32,
    snippet: i32,
    last_log_signature: Option<(usize, String)>,
    usage: Option<UsageSummary>,
    update: UpdateState,
}

/// In-memory (never persisted) state for the self-update flow: the last
/// release found to be newer than the running app (if any), plus
/// checking/installing progress flags. `AppConfig::auto_check_updates` is
/// the only *persisted* piece of update-related state — this struct is
/// everything else.
#[derive(Default)]
struct UpdateState {
    available: Option<AvailableUpdate>,
    checking: bool,
    installing: bool,
    progress: f32,
    error: Option<String>,
}

/// The GUI's central controller: owns the embedded gateway, current
/// configuration, active translations, and the tray's staged update queue,
/// and exposes one async method per user-facing action or background
/// maintenance task.
pub struct Controller {
    ui: Weak<MainWindow>,
    server: Mutex<ServerManager>,
    state: Mutex<State>,
    translations: StdMutex<Translations>,
    pub tray: Arc<StdMutex<TrayShared>>,
}

impl Controller {
    /// Constructs a new controller wrapping a weak handle to `ui` (so the
    /// controller never keeps the window alive on its own), the initial
    /// `translations` table, and the `logs` buffer the embedded gateway
    /// will write its own status lines into once started.
    pub fn new(ui: &MainWindow, translations: Translations, logs: LogBuffer) -> Arc<Self> {
        Arc::new(Self {
            ui: ui.as_weak(),
            server: Mutex::new(ServerManager::new(logs)),
            state: Mutex::new(State::default()),
            translations: StdMutex::new(translations),
            tray: Arc::new(StdMutex::new(TrayShared::default())),
        })
    }

    /// Marshals `f` onto the Slint UI thread and runs it against the live
    /// `MainWindow`, if it still exists. This is the *only* way any
    /// `Controller` method should touch UI state, since Slint properties
    /// must be read/written on the UI thread; every other method in this
    /// file that needs to update the UI goes through this. Silently does
    /// nothing if the window has already been dropped.
    fn with_ui(&self, f: impl FnOnce(MainWindow) + Send + 'static) {
        let _ = self.ui.upgrade_in_event_loop(f);
    }

    /// Looks up a single translated string by key in the active language
    /// (see [`Translations::get`]), falling back to the raw key if the
    /// translations lock is poisoned.
    fn t(&self, key: &str) -> String {
        self.translations
            .lock()
            .map(|tr| tr.get(key).to_string())
            .unwrap_or_else(|_| key.to_string())
    }

    /// Looks up and formats a translated string with `{placeholder}`
    /// substitutions (see [`Translations::format`]), falling back to the
    /// raw key if the translations lock is poisoned.
    fn tf(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.translations
            .lock()
            .map(|tr| tr.format(key, args))
            .unwrap_or_else(|_| key.to_string())
    }

    /// Applies `f` to the shared tray-update staging area, silently doing
    /// nothing if the tray mutex is poisoned (a poisoned tray mutex should
    /// not be able to bring down the rest of the app).
    fn queue_tray<F: FnOnce(&mut TrayShared)>(&self, f: F) {
        if let Ok(mut tray) = self.tray.lock() {
            f(&mut tray);
        }
    }

    /// Stages a fresh set of localized tray menu labels for `main.rs`'s
    /// tray timer to apply on its next tick. Called after bootstrap and
    /// after every language change.
    fn push_tray_labels(&self) {
        let labels = [
            self.t("startServer"),
            self.t("stopServer"),
            self.t("restartServer"),
            self.t("trayShowWindow"),
            self.t("trayHideWindow"),
            self.t("trayQuit"),
        ];
        self.queue_tray(|tray| tray.labels = Some(labels));
    }

    /// Returns the current set of localized tray menu labels, used by
    /// `main.rs` when first constructing the tray icon (before any queued
    /// update exists to pull labels from).
    pub fn tray_labels(&self) -> [String; 6] {
        [
            self.t("startServer"),
            self.t("stopServer"),
            self.t("restartServer"),
            self.t("trayShowWindow"),
            self.t("trayHideWindow"),
            self.t("trayQuit"),
        ]
    }

    /// Borrows a fixed `[String; 6]` label array (as produced by
    /// [`tray_labels`](Self::tray_labels) or the staged `TrayShared::labels`)
    /// as a [`TrayLabels`] struct `tray.rs`'s API expects, in the fixed
    /// start/stop/restart/show/hide/quit order both sides agree on.
    pub fn tray_labels_ref<'a>(labels: &'a [String; 6]) -> TrayLabels<'a> {
        TrayLabels {
            start: &labels[0],
            stop: &labels[1],
            restart: &labels[2],
            show: &labels[3],
            hide: &labels[4],
            quit: &labels[5],
        }
    }

    async fn current_config(&self) -> AppConfig {
        self.state.lock().await.config.clone()
    }

    async fn form_published(&self) -> bool {
        self.state.lock().await.form_published
    }

    async fn config_writable(&self) -> bool {
        self.state.lock().await.config_writable
    }

    async fn is_running(&self) -> bool {
        self.server.lock().await.get_status().status == "running"
    }

    fn set_error(&self, message: String) {
        self.with_ui(move |ui| ui.set_save_error(message.into()));
    }

    /// Runs once at application startup: loads persisted config (or falls
    /// back to defaults if it's missing or unreadable), fills in any
    /// still-missing required fields (client id, API key, auto-discovered
    /// credentials) via [`config::ensure_defaults`], detects/applies the
    /// initial language, persists the config back if anything changed
    /// during that process, applies the auto-launch setting, publishes the
    /// loaded config to the UI, optionally auto-starts the gateway, and
    /// finally spawns two long-running background tasks (periodic
    /// usage/model refresh, and log/status polling) that continue running
    /// for the rest of the process's life.
    ///
    /// This performs filesystem I/O (`config::load_config`,
    /// `config::save_config`), spawns further Tokio tasks, and may start
    /// the embedded gateway (with its own network/process side effects) —
    /// it is meant to be called exactly once, spawned from `main.rs`
    /// immediately after `Controller::new`.
    pub async fn bootstrap(self: &Arc<Self>) {
        let (mut config, config_readable) = match config::load_config().await {
            Ok(config) => (config, true),
            Err(e) => {
                // A corrupt config file must not be silently replaced with
                // defaults and then overwritten on save — that would erase
                // the user's real settings. Fall back to an in-memory
                // default for this session, but remember `config_readable =
                // false` so nothing gets persisted until the user fixes it.
                tracing::error!("failed to load config, not overwriting it: {e}");
                let message = e.clone();
                self.with_ui(move |ui| ui.set_config_error(message.into()));
                (AppConfig::default(), false)
            }
        };

        let mut needs_save = config::ensure_defaults(&mut config) && config_readable;

        let language = match config.language.clone() {
            Some(code) if !code.is_empty() => code,
            _ => {
                let detected = crate::i18n::detect_system_language().to_string();
                config.language = Some(detected.clone());
                needs_save = config_readable;
                detected
            }
        };
        if let Ok(mut tr) = self.translations.lock() {
            tr.set_language(&language);
        }

        if needs_save {
            if let Err(e) = config::save_config(&config).await {
                tracing::warn!("failed to persist initialized config: {e}");
            }
        }

        if let Err(e) = crate::autostart::apply(config.auto_launch) {
            tracing::warn!("auto-launch apply failed (non-fatal): {e}");
        }

        let scan = config::scan_all_credentials().unwrap_or_default();

        {
            let mut state = self.state.lock().await;
            state.config = config.clone();
            state.form_published = true;
            state.config_writable = config_readable;
        }

        self.refresh_translations();
        self.push_tray_labels();
        self.publish_config(&config, &scan.cli_dbs, &scan.creds_files);
        self.refresh_example().await;

        self.with_ui(|ui| ui.set_config_loading(false));

        if config.auto_start_server {
            if config.has_credentials() {
                self.start_server().await;
            } else {
                tracing::warn!("auto-start skipped: no credentials configured");
            }
        }

        // Periodic usage/model refresh, only while the gateway is actually
        // running (there is nothing to refresh otherwise).
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(10 * 60));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if this.is_running().await {
                    this.refresh_usage(false).await;
                    this.refresh_models().await;
                }
            }
        });

        // Log/status poll loop: the UI has no push notification path for
        // gateway log lines or status transitions, so this ticks once per
        // second for the lifetime of the process to pick up changes.
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(1000));
            loop {
                ticker.tick().await;
                this.poll_logs_and_status().await;
            }
        });

        // Periodic self-update check, gated on `auto_check_updates` (the
        // config is re-read every tick, so toggling the setting takes
        // effect on the very next check rather than only after a restart).
        // The first check is delayed (see `UPDATE_CHECK_STARTUP_DELAY`)
        // rather than running immediately, so it doesn't compete with
        // startup work for the network; failures are logged and otherwise
        // silent, since an update check is a convenience, not something
        // that should ever interrupt the user.
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(UPDATE_CHECK_STARTUP_DELAY).await;
            loop {
                if this.current_config().await.auto_check_updates {
                    this.check_for_updates().await;
                }
                tokio::time::sleep(UPDATE_CHECK_INTERVAL).await;
            }
        });
    }

    /// Rebuilds the Slint `Tr` global's strings and the language-picker
    /// model from the currently active language, called after bootstrap
    /// and after every language change. Does nothing if the translations
    /// lock is poisoned.
    fn refresh_translations(&self) {
        let snapshot = match self.translations.lock() {
            Ok(tr) => (
                crate::i18n::LANGUAGES
                    .iter()
                    .map(|(_, label)| SharedString::from(*label))
                    .collect::<Vec<_>>(),
                tr.current_index() as i32,
                tr.snapshot(),
            ),
            Err(_) => return,
        };

        self.with_ui(move |ui| {
            let (labels, index, table) = snapshot;
            ui.set_languages(slint::ModelRc::new(VecModel::from(labels)));
            ui.set_language_index(index);
            crate::tr_generated::apply(&ui.global::<Tr>(), |key| {
                table
                    .get(key)
                    .map(|v| SharedString::from(v.as_str()))
                    .unwrap_or_else(|| SharedString::from(key))
            });
        });
    }

    /// Pushes `config` into the Slint settings form and related UI state
    /// (whether the gateway can be started at all, the "unsaved changes"
    /// flag, and the auto-detected credential file/database lists for the
    /// settings UI's suggestions).
    fn publish_config(&self, config: &AppConfig, cli_dbs: &[String], creds_files: &[String]) {
        let form = ui_state::form_from_config(config);
        let can_start = !config.proxy_api_key.is_empty();
        let cli_dbs: Vec<SharedString> = cli_dbs.iter().map(SharedString::from).collect();
        let creds_files: Vec<SharedString> = creds_files.iter().map(SharedString::from).collect();

        self.with_ui(move |ui| {
            ui.set_form(form);
            ui.set_can_start(can_start);
            ui.set_has_changes(false);
            ui.set_detected_cli_dbs(slint::ModelRc::new(VecModel::from(cli_dbs)));
            ui.set_detected_creds_files(slint::ModelRc::new(VecModel::from(creds_files)));
        });
    }

    /// Starts the embedded gateway using the current configuration.
    ///
    /// Aborts early with a translated error if no credentials are
    /// configured. Before attempting to bind, calls
    /// [`cleanup_stale_occupier_before_start`](Self::cleanup_stale_occupier_before_start)
    /// to proactively reclaim the configured port from a stale Lanius
    /// process (e.g. left over from a crash), since otherwise a perfectly
    /// valid restart could fail on a port the previous instance never
    /// released. If the initial start attempt still fails, tries one more
    /// automatic recovery pass via
    /// [`try_auto_recover_start`](Self::try_auto_recover_start) before
    /// giving up and surfacing the error to the UI.
    pub async fn start_server(self: &Arc<Self>) {
        let config = self.current_config().await;

        if !config.has_credentials() {
            let message = self.t("startServerFailed");
            self.set_error(message);
            return;
        }

        {
            self.state.lock().await.pending = Some(Pending::Start);
        }
        self.with_ui(|ui| {
            ui.set_is_starting(true);
            ui.set_save_error(SharedString::new());
        });

        self.cleanup_stale_occupier_before_start(&config).await;

        let start_result = {
            let mut server = self.server.lock().await;
            server.start(config.clone()).await
        };

        match start_result {
            Ok(_) => {
                self.finish_pending(true).await;
                self.after_start().await;
            }
            Err(e) => {
                tracing::error!("failed to start gateway: {e}");
                if self.try_auto_recover_start(&config).await {
                    self.finish_pending(true).await;
                    self.after_start().await;
                } else {
                    self.finish_pending(false).await;
                    self.set_error(e);
                }
            }
        }
    }

    /// Common post-start work shared by the direct-success and
    /// auto-recovered-success paths in [`start_server`](Self::start_server):
    /// if the gateway ends up actually running, kicks off an initial usage
    /// and model list refresh.
    ///
    /// The embedded gateway seeds its model cache with a built-in fallback
    /// list (no credit-rate/description metadata) and only replaces it with
    /// the live Kiro catalog via a background task that `spawn` does not
    /// wait on — see `lanius_core::server::AppState::initialize`. So this
    /// immediate refresh can race that background fetch and show the
    /// fallback list; a second, delayed refresh (see
    /// [`refresh_models_delayed`](Self::refresh_models_delayed)) is queued
    /// to pick up the live catalog once that fetch has had time to finish,
    /// without the user needing to click refresh manually.
    async fn after_start(self: &Arc<Self>) {
        let running = self.is_running().await;
        self.apply_running(running);
        if running {
            self.refresh_usage(true).await;
            self.refresh_models().await;
            self.refresh_models_delayed(Duration::from_secs(3));
        }
    }

    /// Spawns a background task that waits `delay` then runs
    /// [`refresh_models`](Self::refresh_models) once more, to pick up the
    /// live model catalog once the gateway's background fetch (see
    /// [`after_start`](Self::after_start)) has finished. Fire-and-forget:
    /// callers do not await this.
    fn refresh_models_delayed(self: &Arc<Self>, delay: Duration) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            this.refresh_models().await;
        });
    }

    /// Stops the embedded gateway (a no-op if it isn't running) and clears
    /// runtime-only UI state (model list, usage card) that no longer
    /// applies once the gateway is down.
    pub async fn stop_server(self: &Arc<Self>) {
        {
            self.state.lock().await.pending = Some(Pending::Stop);
        }
        self.with_ui(|ui| ui.set_is_stopping(true));

        let result = {
            let mut server = self.server.lock().await;
            server.stop().await
        };
        if let Err(e) = result {
            tracing::warn!("gateway stop reported an error: {e}");
        }

        self.finish_pending(false).await;
        self.clear_runtime_data();
    }

    /// Stops then restarts the gateway with a short pause in between (to
    /// give the OS a chance to actually release the port before rebinding
    /// it), used both by the manual "Restart" action and after saving
    /// configuration changes that require a restart to take effect.
    pub async fn restart_server(self: &Arc<Self>) {
        self.with_ui(|ui| ui.set_is_restarting(true));
        let config = self.current_config().await;

        self.stop_server().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.start_server().await;

        let _ = config;
        self.with_ui(|ui| ui.set_is_restarting(false));
    }

    /// Clears the [`Pending`] start/stop marker and syncs the UI's
    /// running/starting/stopping flags to `running`, called once a
    /// start/stop attempt has fully resolved (success or failure).
    async fn finish_pending(&self, running: bool) {
        self.state.lock().await.pending = None;
        self.apply_running(running);
    }

    /// Pushes `running` into both the tray (via the staging queue) and the
    /// Slint UI's running/starting/stopping flags. Called from every path
    /// that determines a definitive running state — after a start/stop
    /// completes, and from the periodic status-poll loop when no
    /// start/stop is currently pending.
    fn apply_running(&self, running: bool) {
        self.queue_tray(|tray| tray.running = Some(running));
        self.with_ui(move |ui| {
            ui.set_is_running(running);
            ui.set_is_starting(false);
            ui.set_is_stopping(false);
        });
    }

    /// Resets the model list and usage card (and the tray's credit label)
    /// after the gateway stops, since those figures no longer reflect a
    /// live server.
    fn clear_runtime_data(&self) {
        self.queue_tray(|tray| tray.usage_text = Some("Credit: --".to_string()));
        self.with_ui(|ui| {
            ui.set_models(slint::ModelRc::new(VecModel::<ModelRow>::default()));
            ui.set_usage(usage_placeholder(false, ""));
        });
    }

    /// Looks up whoever currently occupies `port` and, if found, attempts
    /// to terminate it *without* requiring user confirmation
    /// (`terminate_process(..., force: false)`, i.e. it only succeeds if
    /// `process::is_self_owned_process` recognizes the occupier as a
    /// Lanius process). Returns whether a stale process was actually
    /// killed. Never prompts the user, so it can only reclaim a port from
    /// Lanius's own leftover processes, never from an arbitrary unrelated
    /// one.
    async fn kill_occupier_if_any(&self, port: u16) -> bool {
        match process::get_port_occupier(port) {
            Ok(Some(occupier)) => {
                tracing::info!(
                    "port {port} occupied by {} (PID {}), attempting terminate",
                    occupier.process_name,
                    occupier.pid
                );
                match process::terminate_process(occupier.pid, false) {
                    Ok(()) => {
                        tokio::time::sleep(Duration::from_millis(600)).await;
                        true
                    }
                    Err(e) => {
                        tracing::warn!("could not reclaim port: {e}");
                        false
                    }
                }
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!("port lookup failed: {e}");
                false
            }
        }
    }

    /// Proactively tries to reclaim `config.server_port` from a stale
    /// Lanius process *before* the first bind attempt, called at the top of
    /// [`start_server`](Self::start_server). Since terminating a process is
    /// not instantaneous, this retries the kill-and-wait once more if the
    /// first attempt actually killed something, to reduce the odds the port
    /// is still held when the subsequent bind attempt runs.
    async fn cleanup_stale_occupier_before_start(&self, config: &AppConfig) {
        if self.kill_occupier_if_any(config.server_port).await {
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.kill_occupier_if_any(config.server_port).await;
        }
    }

    /// Fallback recovery path invoked from
    /// [`start_server`](Self::start_server) when the *first* bind attempt
    /// already failed (as opposed to
    /// [`cleanup_stale_occupier_before_start`](Self::cleanup_stale_occupier_before_start),
    /// which runs proactively before that first attempt). Only proceeds if
    /// the port's occupier is recognized as self-owned (if it's some
    /// unrelated process, this refuses and returns `false` immediately,
    /// leaving the original bind error to be shown to the user). On
    /// success, stops any partially-started gateway, waits briefly,
    /// retries start, and waits for the gateway to report healthy before
    /// returning `true`.
    async fn try_auto_recover_start(self: &Arc<Self>, config: &AppConfig) -> bool {
        if !self.kill_occupier_if_any(config.server_port).await {
            tracing::warn!("start failed and the port holder is not ours");
            return false;
        }
        {
            let mut server = self.server.lock().await;
            let _ = server.stop().await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        let started = {
            let mut server = self.server.lock().await;
            server.start(config.clone()).await.is_ok()
        };
        if !started {
            return false;
        }
        api::wait_for_health(
            config.probe_host(),
            config.server_port,
            Duration::from_secs(20),
        )
        .await
    }

    /// Called on every keystroke/edit in the settings form: recomputes
    /// whether the edited form actually differs from the persisted
    /// config, and updates the "unsaved changes" flag accordingly,
    /// clearing any stale save success/error/restart-prompt state once a
    /// real change is made. Ignored entirely until the initial config load
    /// has published a baseline form (`form_published`), so spurious
    /// change events during startup don't flag "unsaved changes" against a
    /// config that hasn't loaded yet.
    pub async fn form_changed(&self, form: ConfigForm) {
        if !self.form_published().await {
            return;
        }
        let base = self.current_config().await;
        let updated = ui_state::apply_form(&base, &form);
        let changed = updated != base;
        self.with_ui(move |ui| {
            ui.set_has_changes(changed);
            if changed {
                ui.set_save_success(false);
                ui.set_save_error(SharedString::new());
                ui.set_show_restart_prompt(false);
            }
        });
    }

    /// Handles the "Save" button: applies the form to the base config,
    /// re-applies the auto-launch setting (in case it changed), writes the
    /// result to disk, and republishes the form. If the gateway is
    /// currently running, shows a "restart to apply" prompt instead of
    /// restarting automatically, since a running gateway shouldn't be
    /// silently interrupted by a settings save the user may not expect to
    /// take effect immediately. On success while the gateway is *not*
    /// running, the success indicator auto-clears itself after 3 seconds
    /// via a background task. Ignored (with a warning) if the initial
    /// config load hasn't published a baseline form yet.
    pub async fn save_config(self: &Arc<Self>, form: ConfigForm) {
        if !self.form_published().await {
            tracing::warn!("ignoring save: configuration has not finished loading");
            return;
        }

        self.with_ui(|ui| {
            ui.set_is_saving(true);
            ui.set_save_error(SharedString::new());
            ui.set_save_success(false);
        });

        let base = self.current_config().await;
        let updated = ui_state::apply_form(&base, &form);

        if let Err(e) = crate::autostart::apply(updated.auto_launch) {
            tracing::warn!("auto-launch apply failed: {e}");
        }

        match config::save_config(&updated).await {
            Ok(()) => {
                let running = self.is_running().await;
                {
                    let mut state = self.state.lock().await;
                    state.config = updated.clone();
                    state.config_writable = true;
                }
                let can_start = !updated.proxy_api_key.is_empty();
                let form = ui_state::form_from_config(&updated);
                self.with_ui(move |ui| {
                    ui.set_is_saving(false);
                    ui.set_save_success(true);
                    ui.set_has_changes(false);
                    ui.set_form(form);
                    ui.set_can_start(can_start);
                    ui.set_show_restart_prompt(running);
                });
                self.refresh_example().await;

                if !running {
                    let this = Arc::clone(self);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        this.with_ui(|ui| ui.set_save_success(false));
                    });
                }
            }
            Err(e) => {
                let message = if e.is_empty() {
                    self.t("saveFailed")
                } else {
                    e
                };
                self.with_ui(move |ui| {
                    ui.set_is_saving(false);
                    ui.set_save_error(message.into());
                });
            }
        }
    }

    /// Regenerates both the real and display (masked-key) versions of the
    /// currently selected API example snippet from the latest config, and
    /// pushes both into the UI. Called after bootstrap, after every
    /// config save, and whenever the selected flavor/snippet tab changes.
    pub async fn refresh_example(&self) {
        let (config, flavor, snippet) = {
            let state = self.state.lock().await;
            (state.config.clone(), state.api_flavor, state.snippet)
        };
        let flavor = ApiFlavor::from_index(flavor);
        let snippet = Snippet::from_index(snippet);
        let code = examples::render(
            flavor,
            snippet,
            &config.server_host,
            config.server_port,
            &config.proxy_api_key,
        );
        let masked_key = examples::mask_key(&config.proxy_api_key);
        let display = examples::render(
            flavor,
            snippet,
            &config.server_host,
            config.server_port,
            &masked_key,
        );
        self.with_ui(move |ui| {
            ui.set_example_code(code.into());
            ui.set_example_code_display(display.into());
        });
    }

    /// Records the user's chosen API-flavor/snippet-language tab and
    /// regenerates the displayed example accordingly.
    pub async fn set_example_selection(&self, flavor: i32, snippet: i32) {
        {
            let mut state = self.state.lock().await;
            state.api_flavor = flavor;
            state.snippet = snippet;
        }
        self.refresh_example().await;
    }

    /// Switches the active UI language by picker index: updates the
    /// in-memory `Translations`, refreshes the `Tr` global and tray labels,
    /// and — if the resolved language code actually differs from what's
    /// persisted — updates and (if writable) saves the config so the
    /// choice survives a restart. Also triggers an immediate log/status
    /// poll so any log lines rendered during this call reflect the new
    /// language's formatting where relevant.
    pub async fn set_language(self: &Arc<Self>, index: i32) {
        if let Ok(mut tr) = self.translations.lock() {
            tr.set_language_by_index(index.max(0) as usize);
        }
        let code = self
            .translations
            .lock()
            .map(|tr| tr.current().to_string())
            .unwrap_or_else(|_| "en".to_string());

        self.refresh_translations();
        self.push_tray_labels();

        let mut config = self.current_config().await;
        if config.language.as_deref() != Some(code.as_str()) {
            config.language = Some(code);
            self.state.lock().await.config = config.clone();
            if self.config_writable().await {
                if let Err(e) = config::save_config(&config).await {
                    tracing::warn!("failed to persist language: {e}");
                }
            } else {
                tracing::warn!("language not persisted: existing config could not be read");
            }
        }
        self.poll_logs_and_status().await;
    }

    /// Called once per second by the background poll task (and once
    /// on-demand from a few other call sites, e.g. after clearing logs or
    /// changing language): pulls the latest log lines and status from the
    /// `ServerManager`, and — only if the log content actually changed
    /// since the last poll (tracked via a cheap `(len, last_line)`
    /// signature rather than diffing full content) — reprocesses and
    /// republishes the log view. Also syncs the running-state UI flags
    /// from the server's real status, but only when no manual start/stop
    /// is currently [`Pending`], so this polling loop never fights with an
    /// in-flight manual operation over the UI's starting/stopping
    /// indicators.
    async fn poll_logs_and_status(&self) {
        let (lines, status) = {
            let server = self.server.lock().await;
            (server.get_logs(), server.get_status())
        };

        let signature = (lines.len(), lines.last().cloned().unwrap_or_default());
        let changed = {
            let mut state = self.state.lock().await;
            let changed = state.last_log_signature.as_ref() != Some(&signature);
            if changed {
                state.last_log_signature = Some(signature);
            }
            changed
        };

        if changed {
            let processed = logs::process(&lines);
            let rows = ui_state::log_rows(&processed);
            let count_text = self.tf("eventsCaptured", &[("count", &lines.len().to_string())]);
            self.with_ui(move |ui| {
                ui.set_logs(slint::ModelRc::new(VecModel::from(rows)));
                ui.set_logs_count_text(count_text.into());
            });
        }

        let running = status.status == "running";

        if self.state.lock().await.pending.is_none() {
            self.apply_running(running);
        }
    }

    /// Refreshes the usage/credit dashboard card from the gateway's
    /// `/usage` endpoint. No-ops immediately if there's no API key
    /// configured or the gateway isn't running (there's nothing to fetch).
    ///
    /// If usage has never been successfully loaded yet, shows the
    /// "loading" placeholder immediately; if a previous summary exists,
    /// keeps showing it but flags it as `loading` (so the UI can show a
    /// subtle in-place refresh indicator rather than blanking the card).
    /// When `initial` is `true` (the first refresh right after a
    /// successful start), retries up to 5 times with a 3-second delay
    /// between attempts, since the gateway may need a moment to fully
    /// warm up its upstream connection immediately after starting;
    /// otherwise (periodic/manual refreshes) it tries only once, on the
    /// assumption that a gateway that has been running for a while should
    /// already be responsive. On final failure, falls back to an error
    /// placeholder only if nothing had ever loaded before — an
    /// already-displayed summary is left showing rather than being
    /// replaced with an error, since stale-but-real numbers are more
    /// useful than an error message.
    pub async fn refresh_usage(self: &Arc<Self>, initial: bool) {
        let config = self.current_config().await;
        if config.proxy_api_key.is_empty() || !self.is_running().await {
            return;
        }

        let existing = self.state.lock().await.usage.clone();
        let already_loaded = existing.is_some();
        if !already_loaded {
            self.with_ui(|ui| ui.set_usage(usage_placeholder(true, "")));
        } else if let Some(summary) = existing {
            let mut view = ui_state::usage_view(&summary);
            view.loading = true;
            self.with_ui(move |ui| ui.set_usage(view));
        }

        let attempts = if initial { 5 } else { 1 };
        for attempt in 0..attempts {
            match api::fetch_usage(
                config.probe_host(),
                config.server_port,
                &config.proxy_api_key,
            )
            .await
            {
                Ok(summary) => {
                    if let Some(label) = summary.tray_label() {
                        self.queue_tray(|tray| tray.usage_text = Some(label));
                    }
                    let view = ui_state::usage_view(&summary);
                    self.state.lock().await.usage = Some(summary);
                    self.with_ui(move |ui| ui.set_usage(view));
                    return;
                }
                Err(e) => {
                    if attempt + 1 == attempts {
                        tracing::debug!("usage fetch failed: {e}");
                        if !already_loaded {
                            self.with_ui(move |ui| ui.set_usage(usage_placeholder(false, &e)));
                        } else {
                            if let Some(summary) = self.state.lock().await.usage.clone() {
                                let view = ui_state::usage_view(&summary);
                                self.with_ui(move |ui| ui.set_usage(view));
                            }
                        }
                    } else {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        }
    }

    /// Refreshes the model list from the gateway's `/v1/models` endpoint.
    /// No-ops if there's no API key configured or the gateway isn't
    /// running. Failures are logged at debug level and otherwise silent
    /// (leaving whatever model list was previously shown, if any), since a
    /// model list is a secondary, non-critical piece of the dashboard.
    pub async fn refresh_models(&self) {
        let config = self.current_config().await;
        if config.proxy_api_key.is_empty() || !self.is_running().await {
            return;
        }

        self.with_ui(|ui| ui.set_models_loading(true));
        match api::fetch_models(
            config.probe_host(),
            config.server_port,
            &config.proxy_api_key,
        )
        .await
        {
            Ok(models) => {
                let models: Vec<ModelRow> = models
                    .into_iter()
                    .map(|model| ModelRow {
                        id: SharedString::from(model.id),
                        rate_text: SharedString::from(model.rate_text),
                        description: SharedString::from(model.description),
                        supports_thinking: model.supports_thinking,
                    })
                    .collect();
                self.with_ui(move |ui| {
                    ui.set_models(slint::ModelRc::new(VecModel::from(models)));
                    ui.set_models_loading(false);
                });
            }
            Err(e) => {
                tracing::debug!("model list fetch failed: {e}");
                self.with_ui(|ui| ui.set_models_loading(false));
            }
        }
    }

    /// Runs at application shutdown (after the Slint event loop exits):
    /// stops the embedded gateway so it doesn't linger as an orphaned
    /// in-process task once the GUI process itself is about to exit.
    pub async fn shutdown(self: &Arc<Self>) {
        let mut server = self.server.lock().await;
        let _ = server.stop().await;
    }

    /// Dispatches a [`TrayCommand`] (received from the tray menu, via
    /// `main.rs`'s tray timer) to the corresponding controller action.
    /// `ShowWindow`/`HideWindow` don't act directly — they stage a request
    /// via [`queue_tray`](Self::queue_tray) for `main.rs` to apply, since
    /// only the main/event-loop thread can touch the `MainWindow`'s
    /// visibility, whereas `Quit` first runs a full [`shutdown`](Self::shutdown)
    /// before staging the quit request, so the gateway is stopped cleanly
    /// before the event loop actually exits.
    pub async fn handle_tray_command(self: &Arc<Self>, command: TrayCommand) {
        match command {
            TrayCommand::StartServer => self.start_server().await,
            TrayCommand::StopServer => self.stop_server().await,
            TrayCommand::RestartServer => self.restart_server().await,
            TrayCommand::ShowWindow => self.queue_tray(|tray| tray.show_window = true),
            TrayCommand::HideWindow => self.queue_tray(|tray| tray.hide_window = true),
            TrayCommand::Quit => {
                self.shutdown().await;
                self.queue_tray(|tray| tray.quit = true);
            }
        }
    }

    /// Returns the full captured log buffer as ANSI-stripped plain text,
    /// for the "export logs" file-save action in `main.rs`.
    pub async fn export_text(&self) -> String {
        let server = self.server.lock().await;
        logs::export_text(&server.get_logs())
    }

    /// Clears the log buffer and the cached "last seen" log signature (so
    /// the next status poll doesn't think nothing changed just because the
    /// buffer is now empty), then immediately re-polls so the UI's log
    /// view reflects the clear right away rather than waiting for the next
    /// scheduled tick.
    pub async fn clear_logs(&self) {
        {
            let mut server = self.server.lock().await;
            server.clear_logs();
        }
        {
            let mut state = self.state.lock().await;
            state.last_log_signature = None;
        }
        self.poll_logs_and_status().await;
    }

    /// Pushes the current [`UpdateState`] into the Slint UI's
    /// update-related properties (mirrored on both the sidebar's pill and
    /// the settings page's "Updates" card).
    fn publish_update_state(&self, state: &UpdateState) {
        let available = state.available.is_some();
        let latest_version = state
            .available
            .as_ref()
            .map(|u| u.version.to_string())
            .unwrap_or_default();
        let checking = state.checking;
        let installing = state.installing;
        let progress = state.progress;
        let error = state.error.clone().unwrap_or_default();
        self.with_ui(move |ui| {
            ui.set_update_available(available);
            ui.set_update_latest_version(latest_version.into());
            ui.set_update_checking(checking);
            ui.set_update_installing(installing);
            ui.set_update_progress(progress);
            ui.set_update_error(error.into());
        });
    }

    /// Checks GitHub for a newer release, called both by the periodic
    /// background task (see [`bootstrap`](Self::bootstrap)) and the
    /// "Check Now" button in Settings. Failures are logged and reflected in
    /// the UI's error text, but otherwise non-fatal — an update check is a
    /// convenience, never something that should block or crash the app.
    pub async fn check_for_updates(self: &Arc<Self>) {
        {
            let mut state = self.state.lock().await;
            state.update.checking = true;
            state.update.error = None;
            self.publish_update_state(&state.update);
        }

        let result = updater::check().await;

        let mut state = self.state.lock().await;
        state.update.checking = false;
        match result {
            Ok(available) => {
                state.update.available = available;
            }
            Err(e) => {
                tracing::warn!("update check failed: {e}");
                state.update.error = Some(e.to_string());
            }
        }
        self.publish_update_state(&state.update);
    }

    /// Downloads, verifies, and installs the update found by
    /// [`check_for_updates`](Self::check_for_updates), then relaunches into
    /// it. No-ops if no update is currently known to be available (the UI
    /// only shows the "Update & Restart" action once one is).
    ///
    /// On platforms/situations `updater::install` can't handle (see its
    /// docs — anything other than a normally-installed macOS app bundle),
    /// falls back to opening the release's GitHub page in a browser so the
    /// user can still update manually.
    pub async fn install_update(self: &Arc<Self>) {
        let Some(update) = self.state.lock().await.update.available.clone() else {
            return;
        };

        {
            let mut state = self.state.lock().await;
            state.update.installing = true;
            state.update.progress = 0.0;
            state.update.error = None;
            self.publish_update_state(&state.update);
        }

        let this = Arc::clone(self);
        let progress_update = move |downloaded: u64, total: u64| {
            if total > 0 {
                let fraction = downloaded as f32 / total as f32;
                let this = Arc::clone(&this);
                // `install` calls this synchronously from inside the async
                // download loop; queue the UI update rather than blocking
                // that loop on an `.await` for every chunk.
                tokio::spawn(async move {
                    let mut state = this.state.lock().await;
                    state.update.progress = fraction;
                    this.publish_update_state(&state.update);
                });
            }
        };

        match updater::install(&update, progress_update).await {
            Ok(installed_path) => {
                self.shutdown().await;
                if let Err(e) = updater::relaunch(&installed_path) {
                    tracing::error!("update installed but relaunch failed: {e}");
                    let mut state = self.state.lock().await;
                    state.update.installing = false;
                    state.update.error = Some(e.to_string());
                    self.publish_update_state(&state.update);
                    return;
                }
                self.queue_tray(|tray| tray.quit = true);
            }
            Err(updater::UpdaterError::UnsupportedPlatform) => {
                tracing::info!("in-app update unsupported here; opening the release page instead");
                updater::open_url(&update.release.html_url);
                let mut state = self.state.lock().await;
                state.update.installing = false;
                self.publish_update_state(&state.update);
            }
            Err(e) => {
                tracing::error!("update install failed: {e}");
                let mut state = self.state.lock().await;
                state.update.installing = false;
                state.update.error = Some(e.to_string());
                self.publish_update_state(&state.update);
            }
        }
    }

    /// Opens the GitHub release page in a browser for the currently known
    /// available update, if any — the fallback action when an in-app
    /// install can't proceed (unsupported platform, permission failure).
    pub async fn open_release_page(&self) {
        if let Some(update) = self.state.lock().await.update.available.clone() {
            updater::open_url(&update.release.html_url);
        }
    }
}

/// Writes `text` to the system clipboard, logging (rather than
/// propagating) any failure, since every call site here is a "copy"
/// button click where there is no meaningful recovery action beyond
/// letting the user know via logs if the platform clipboard is
/// unavailable.
pub fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new() {
        Ok(mut clipboard) => {
            if let Err(e) = clipboard.set_text(text.to_string()) {
                tracing::warn!("clipboard write failed: {e}");
            }
        }
        Err(e) => tracing::warn!("clipboard unavailable: {e}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn copy_to_clipboard_writes_the_text() {
        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            eprintln!("no clipboard available, skipping");
            return;
        };
        let previous = clipboard.get_text().ok();

        super::copy_to_clipboard("claude-sonnet-4-6");
        assert_eq!(
            arboard::Clipboard::new()
                .and_then(|mut c| c.get_text())
                .ok()
                .as_deref(),
            Some("claude-sonnet-4-6")
        );

        if let Some(previous) = previous {
            let _ = clipboard.set_text(previous);
        }
    }
}
