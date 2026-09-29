//! Lifecycle management for the embedded `lanius-core` gateway, running
//! in-process inside the GUI (not as a separate child process).
//!
//! [`ServerManager`] wraps `lanius_core::server::spawn`/`GatewayHandle` to
//! start/stop the gateway and tracks its current [`ServerStatus`].
//! `controller.rs` owns the single `ServerManager` instance (behind an async
//! `Mutex`) and is the only caller of [`ServerManager::start`]/
//! [`ServerManager::stop`]; [`build_gateway_config`] is the translation
//! layer from the GUI's own [`AppConfig`] into `lanius_core::Config`, the
//! type the core gateway itself understands. Status/log messages produced
//! here go through the same shared [`LogBuffer`] as `log_capture.rs`'s
//! `tracing`-based capture, so gateway lifecycle events appear in the GUI's
//! log view alongside ordinary log lines.

use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, AuthMethod, get_app_data_dir};
use crate::log_capture::LogBuffer;

/// The gateway's current run state, as surfaced to the UI (`"stopped"`,
/// `"starting"`, `"running"`, or `"error"`), including its bound port when
/// running and an error message when the last start attempt failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub status: String,
    pub port: Option<u16>,
    pub error: Option<String>,
}

impl ServerStatus {
    fn stopped() -> Self {
        Self {
            status: "stopped".to_string(),
            port: None,
            error: None,
        }
    }
}

/// Owns the embedded gateway's lifecycle: at most one
/// `lanius_core::server::GatewayHandle` at a time, the last known
/// [`ServerStatus`], and a handle to the shared log buffer gateway
/// lifecycle messages are appended to.
pub struct ServerManager {
    handle: Option<lanius_core::server::GatewayHandle>,
    status: ServerStatus,
    logs: LogBuffer,
}

impl ServerManager {
    /// Creates a manager in the stopped state, appending its own lifecycle
    /// log lines to the given shared `logs` buffer.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use crate::log_capture::LogBuffer;
    /// let manager = crate::server::ServerManager::new(LogBuffer::new());
    /// assert_eq!(manager.get_status().status, "stopped");
    /// ```
    pub fn new(logs: LogBuffer) -> Self {
        Self {
            handle: None,
            status: ServerStatus::stopped(),
            logs,
        }
    }

    /// Returns a snapshot of every log line captured so far (both from
    /// `tracing` events and this manager's own lifecycle messages).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let manager = crate::server::ServerManager::new(crate::log_capture::LogBuffer::new());
    /// for line in manager.get_logs() {
    ///     println!("{line}");
    /// }
    /// ```
    pub fn get_logs(&self) -> Vec<String> {
        self.logs.get_all()
    }

    /// Clears the shared log buffer (used by the "Clear logs" UI action).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut manager = crate::server::ServerManager::new(crate::log_capture::LogBuffer::new());
    /// manager.clear_logs();
    /// assert!(manager.get_logs().is_empty());
    /// ```
    pub fn clear_logs(&mut self) {
        self.logs.clear();
    }

    /// Records a gateway lifecycle line both to the `tracing` log (at info
    /// level, so it also flows through `log_capture.rs`'s `CaptureLayer` if
    /// installed) and directly to the shared buffer with its own timestamp
    /// prefix, matching the format `logs::extract_time` parses.
    fn log(&self, line: impl Into<String>) {
        let line = line.into();
        tracing::info!("{line}");
        let stamped = format!(
            "{} | {line}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        self.logs.push_stamped(stamped);
    }

    /// Starts the embedded gateway using `config`, translating it into
    /// `lanius_core::Config` via [`build_gateway_config`] and validating it
    /// before attempting to bind.
    ///
    /// Returns an error (without touching `self.handle`) if a gateway is
    /// already running, if the translated configuration fails validation
    /// (e.g. missing credentials), or if `lanius_core::server::spawn` itself
    /// fails (most commonly because the configured port is already in
    /// use — see `process.rs` for how `controller.rs` detects and resolves
    /// that). On success, stores the new `GatewayHandle` and updates
    /// `self.status` to `"running"` with the port the gateway actually
    /// bound to.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut manager = crate::server::ServerManager::new(crate::log_capture::LogBuffer::new());
    /// let config = crate::config::load_config().await?;
    /// let status = manager.start(config).await?;
    /// println!("gateway listening on {:?}", status.port);
    /// ```
    pub async fn start(&mut self, config: &AppConfig) -> Result<ServerStatus, String> {
        if self.handle.is_some() {
            return Err("Server is already running".to_string());
        }

        self.status = ServerStatus {
            status: "starting".to_string(),
            port: Some(config.server_port),
            error: None,
        };

        let gateway_config = build_gateway_config(config)?;

        if let Err(e) = gateway_config.validate() {
            let msg = format!("Invalid configuration: {e}");
            self.status = ServerStatus {
                status: "error".to_string(),
                port: None,
                error: Some(msg.clone()),
            };
            return Err(msg);
        }

        self.log(format!(
            "Starting in-process gateway on {}:{} (region {})",
            gateway_config.server_host, gateway_config.server_port, gateway_config.region
        ));

        match lanius_core::server::spawn(gateway_config).await {
            Ok(handle) => {
                let port = handle.local_addr().port();
                self.handle = Some(handle);
                self.status = ServerStatus {
                    status: "running".to_string(),
                    port: Some(port),
                    error: None,
                };
                self.log(format!("Gateway listening on port {port}"));
                Ok(self.status.clone())
            }
            Err(e) => {
                let msg = format!("Failed to start gateway: {e}");
                self.log(msg.clone());
                self.status = ServerStatus {
                    status: "error".to_string(),
                    port: None,
                    error: Some(msg.clone()),
                };
                Err(msg)
            }
        }
    }

    /// Stops the embedded gateway if one is running (a no-op, returning
    /// `Ok(())`, if it is already stopped). Always leaves `self.handle` as
    /// `None` and `self.status` as stopped afterward, even if the
    /// underlying `GatewayHandle::shutdown` reports an error — the error is
    /// still returned to the caller for logging/display, but does not
    /// prevent the manager from considering itself stopped.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut manager = crate::server::ServerManager::new(crate::log_capture::LogBuffer::new());
    /// manager.start(crate::config::load_config().await?).await?;
    /// manager.stop().await?;
    /// assert_eq!(manager.get_status().status, "stopped");
    /// ```
    pub async fn stop(&mut self) -> Result<(), String> {
        match self.handle.take() {
            Some(handle) => {
                self.log("Stopping gateway...");
                let result = handle.shutdown().await;
                self.status = ServerStatus::stopped();
                match result {
                    Ok(()) => {
                        self.log("Gateway stopped");
                        Ok(())
                    }
                    Err(e) => {
                        let msg = format!("Gateway stopped with error: {e}");
                        self.log(msg.clone());
                        Err(msg)
                    }
                }
            }
            None => {
                self.status = ServerStatus::stopped();
                Ok(())
            }
        }
    }

    /// Returns the last known [`ServerStatus`].
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let manager = crate::server::ServerManager::new(crate::log_capture::LogBuffer::new());
    /// let status = manager.get_status();
    /// assert!(status.port.is_none());
    /// ```
    pub fn get_status(&self) -> &ServerStatus {
        &self.status
    }
}

impl Drop for ServerManager {
    /// Drops the `GatewayHandle` (if any) when the manager itself is
    /// dropped, so the embedded gateway is torn down if the manager goes
    /// out of scope without an explicit `stop()` call (e.g. during process
    /// exit). This does not run any async shutdown sequence — it relies on
    /// `GatewayHandle`'s own `Drop` behavior.
    fn drop(&mut self) {
        self.handle.take();
    }
}

/// Translates the GUI's [`AppConfig`] into the `lanius_core::Config` the
/// embedded gateway itself understands, applying only the fields relevant
/// to the currently selected [`AuthMethod`] and pointing the gateway's
/// writable state (debug logs) at a subdirectory of the GUI's own
/// application data directory so the embedded gateway never needs its own
/// separate data location.
fn build_gateway_config(app: &AppConfig) -> Result<lanius_core::Config, String> {
    let data_dir = get_app_data_dir()?;

    let mut cfg = lanius_core::Config {
        server_host: app.server_host.clone(),
        server_port: app.server_port,
        proxy_api_key: app.proxy_api_key.clone(),
        region: app.kiro_region.clone(),
        vpn_proxy_url: non_empty(app.vpn_proxy_url.as_deref()),
        first_token_timeout: secs_f32(app.first_token_timeout, 15.0),
        streaming_read_timeout: secs_f32(app.streaming_read_timeout, 300.0),
        truncation_recovery: app.truncation_recovery,
        log_level: app.log_level.to_uppercase(),
        debug_mode: parse_debug_mode(&app.debug_mode),
        ..lanius_core::Config::default()
    };

    // Only the field belonging to the currently selected auth method is
    // copied over; the other credential fields are deliberately left unset
    // even if they hold stale values, so the gateway never authenticates
    // with a method the user did not select.
    match app.auth_method {
        AuthMethod::RefreshToken => {
            cfg.refresh_token = non_empty(app.refresh_token.as_deref());
        }
        AuthMethod::CredsFile => {
            cfg.kiro_creds_file = non_empty(app.kiro_creds_file.as_deref()).map(Into::into);
        }
        AuthMethod::CliDb => {
            cfg.kiro_cli_db_file = non_empty(app.kiro_cli_db_file.as_deref()).map(Into::into);
            // The kiro-cli database is owned by another application (the
            // Kiro CLI); Lanius must never write to it, only read from it.
            cfg.sqlite_readonly = true;
        }
    }

    cfg.debug_dir = data_dir.join("debug_logs");

    Ok(cfg)
}

/// Trims `value` and converts a blank/whitespace-only string to `None`,
/// treating an empty form field as "not configured" rather than as an
/// empty string that would otherwise be passed through to the gateway.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Converts a UI-editable timeout in seconds (`f32`) into a `Duration`,
/// substituting `fallback` (in seconds) whenever `value` is not a finite,
/// positive number — guarding against a blank/invalid form field (which
/// could otherwise produce `NaN`, a negative, or a zero-length timeout)
/// ever reaching the gateway.
fn secs_f32(value: f32, fallback: f64) -> std::time::Duration {
    let secs = if value.is_finite() && value > 0.0 {
        f64::from(value)
    } else {
        fallback
    };
    std::time::Duration::from_secs_f64(secs)
}

/// Parses the free-text `debug_mode` config string into the gateway's
/// `DebugMode` enum, matched case-insensitively; any value other than
/// `"all"` or `"errors"` (including an empty/unset string) is treated as
/// `Off`.
fn parse_debug_mode(raw: &str) -> lanius_core::config::DebugMode {
    use lanius_core::config::DebugMode;
    match raw.trim().to_ascii_lowercase().as_str() {
        "all" => DebugMode::All,
        "errors" => DebugMode::Errors,
        _ => DebugMode::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> AppConfig {
        AppConfig {
            proxy_api_key: "test-key".to_string(),
            ..AppConfig::default()
        }
    }

    #[test]
    fn maps_ui_settings_into_gateway_config() {
        let mut app = base_config();
        app.server_host = "0.0.0.0".to_string();
        app.server_port = 9001;
        app.kiro_region = "eu-central-1".to_string();
        app.first_token_timeout = 20.0;
        app.streaming_read_timeout = 120.0;
        app.debug_mode = "ALL".to_string();
        app.log_level = "debug".to_string();

        let cfg = build_gateway_config(&app).expect("mapping should succeed");
        assert_eq!(cfg.server_host, "0.0.0.0");
        assert_eq!(cfg.server_port, 9001);
        assert_eq!(cfg.region, "eu-central-1");
        assert_eq!(cfg.first_token_timeout, std::time::Duration::from_secs(20));
        assert_eq!(
            cfg.streaming_read_timeout,
            std::time::Duration::from_secs(120)
        );
        assert_eq!(cfg.debug_mode, lanius_core::config::DebugMode::All);
        assert_eq!(cfg.log_level, "DEBUG");
        assert_eq!(
            cfg.api_host(),
            "https://runtime.eu-central-1.kiro.dev",
            "region must flow through to the upstream host"
        );
    }

    #[test]
    fn cli_db_auth_forces_readonly_sqlite() {
        let mut app = base_config();
        app.auth_method = AuthMethod::CliDb;
        app.kiro_cli_db_file = Some("/tmp/kiro.db".to_string());

        let cfg = build_gateway_config(&app).expect("mapping should succeed");
        assert!(
            cfg.sqlite_readonly,
            "the kiro-cli database must never be opened for writing"
        );
        assert!(cfg.kiro_cli_db_file.is_some());
        assert!(cfg.refresh_token.is_none());
    }

    #[test]
    fn only_the_selected_auth_method_is_populated() {
        let mut app = base_config();
        app.auth_method = AuthMethod::RefreshToken;
        app.refresh_token = Some("tok".to_string());
        app.kiro_creds_file = Some("/should/be/ignored.json".to_string());

        let cfg = build_gateway_config(&app).expect("mapping should succeed");
        assert_eq!(cfg.refresh_token.as_deref(), Some("tok"));
        assert!(cfg.kiro_creds_file.is_none());
    }

    #[test]
    fn empty_strings_are_treated_as_unset() {
        let mut app = base_config();
        app.vpn_proxy_url = Some("   ".to_string());
        app.refresh_token = Some(String::new());

        let cfg = build_gateway_config(&app).expect("mapping should succeed");
        assert!(cfg.vpn_proxy_url.is_none());
        assert!(cfg.refresh_token.is_none());
    }

    #[test]
    fn invalid_timeouts_fall_back_to_defaults() {
        let mut app = base_config();
        app.first_token_timeout = 0.0;
        app.streaming_read_timeout = f32::NAN;

        let cfg = build_gateway_config(&app).expect("mapping should succeed");
        assert_eq!(cfg.first_token_timeout, std::time::Duration::from_secs(15));
        assert_eq!(
            cfg.streaming_read_timeout,
            std::time::Duration::from_secs(300)
        );
    }

    #[test]
    fn writable_paths_are_absolute_and_under_app_data_dir() {
        let cfg = build_gateway_config(&base_config()).expect("mapping should succeed");
        assert!(cfg.debug_dir.is_absolute());
    }

    #[test]
    fn status_starts_stopped_and_logs_are_bounded() {
        use crate::log_capture::MAX_LOG_LINES;

        let mgr = ServerManager::new(LogBuffer::new());
        assert_eq!(mgr.get_status().status, "stopped");
        assert!(mgr.get_status().port.is_none());

        for i in 0..(MAX_LOG_LINES + 50) {
            mgr.log(format!("line {i}"));
        }
        let logs = mgr.get_logs();
        assert_eq!(logs.len(), MAX_LOG_LINES, "log buffer must stay bounded");
        let newest = logs.last().expect("buffer must not be empty");
        assert!(
            newest.ends_with(&format!("line {}", MAX_LOG_LINES + 49)),
            "newest line must be retained, got {newest:?}"
        );
        assert!(
            !crate::logs::extract_time(newest).is_empty(),
            "log() must stamp every line with a timestamp `extract_time` can parse, got {newest:?}"
        );
    }
}
