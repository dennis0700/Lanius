//! Persisted application configuration: the [`AppConfig`] struct, its
//! on-disk JSON load/save, credential auto-discovery, and API-key
//! generation.
//!
//! This is the single source of truth for user-editable settings (auth
//! method and credentials, server bind address/port, region, timeouts,
//! auto-launch/auto-start toggles, language, etc.). `main.rs`
//! never touches this directly; `controller.rs` owns the in-memory
//! `AppConfig` and is the only caller of [`load_config`]/[`save_config`],
//! while `ui_state.rs` converts between `AppConfig` and the Slint settings
//! form. `server.rs` reads a `&AppConfig` to build the embedded gateway's
//! own `lanius_core::Config` when starting the server.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

/// Which credential source the gateway should authenticate with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    RefreshToken,
    CredsFile,
    CliDb,
}

impl AuthMethod {
    /// Returns the stable string identifier used both in serialized JSON
    /// (implicitly, via the `snake_case` derive) and in the settings form's
    /// UI value, so this and the `serde` representation must stay in sync.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use crate::config::AuthMethod;
    /// assert_eq!(AuthMethod::CliDb.as_str(), "cli_db");
    /// ```
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::RefreshToken => "refresh_token",
            AuthMethod::CredsFile => "creds_file",
            AuthMethod::CliDb => "cli_db",
        }
    }

    /// Parses a UI/string value back into an [`AuthMethod`], defaulting to
    /// [`AuthMethod::RefreshToken`] for any unrecognized value rather than
    /// failing, since this is used to interpret user-editable form input
    /// that must never panic.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use crate::config::AuthMethod;
    /// assert_eq!(AuthMethod::from_str_or_default("creds_file"), AuthMethod::CredsFile);
    /// assert_eq!(AuthMethod::from_str_or_default("nonsense"), AuthMethod::RefreshToken);
    /// ```
    pub fn from_str_or_default(raw: &str) -> Self {
        match raw {
            "creds_file" => AuthMethod::CredsFile,
            "cli_db" => AuthMethod::CliDb,
            _ => AuthMethod::RefreshToken,
        }
    }
}

/// The full set of persisted application settings, serialized to
/// `config.json` under the app's data directory (see
/// [`get_app_data_dir`]). Fields marked `#[serde(default...)]` were added
/// after the initial release and must tolerate being absent from
/// previously saved config files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub auth_method: AuthMethod,
    pub refresh_token: Option<String>,
    pub kiro_creds_file: Option<String>,
    pub kiro_cli_db_file: Option<String>,

    pub proxy_api_key: String,

    pub server_host: String,
    pub server_port: u16,
    pub kiro_region: String,

    pub vpn_proxy_url: Option<String>,
    pub first_token_timeout: f32,
    pub streaming_read_timeout: f32,
    pub truncation_recovery: bool,
    pub log_level: String,
    pub debug_mode: String,

    #[serde(default)]
    pub auto_launch: bool,
    #[serde(default)]
    pub auto_start_server: bool,

    #[serde(default)]
    pub client_id: Option<String>,

    #[serde(default)]
    pub language: Option<String>,

    /// Whether the app periodically checks GitHub for a newer release (see
    /// `controller.rs`'s update-check background task). Defaults to `true`
    /// via `#[serde(default = "default_true")]` so existing config files
    /// (saved before this field existed) opt in rather than silently
    /// disabling checks.
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            auth_method: AuthMethod::RefreshToken,
            refresh_token: None,
            kiro_creds_file: None,
            kiro_cli_db_file: None,
            proxy_api_key: String::new(),
            server_host: "127.0.0.1".to_string(),
            server_port: 18000,
            kiro_region: "us-east-1".to_string(),
            vpn_proxy_url: None,
            first_token_timeout: 15.0,
            streaming_read_timeout: 300.0,
            truncation_recovery: true,
            log_level: "INFO".to_string(),
            debug_mode: "off".to_string(),
            auto_launch: false,
            auto_start_server: false,
            client_id: None,
            language: None,
            auto_check_updates: true,
        }
    }
}

impl AppConfig {
    /// Reports whether a usable credential is configured for the currently
    /// selected [`AuthMethod`] — i.e. whether starting the gateway is
    /// expected to succeed from a credentials standpoint. Only the field
    /// relevant to the active auth method is checked; a stale value left
    /// over in an unused field (e.g. a `refresh_token` set while
    /// `auth_method` is `CliDb`) is intentionally ignored.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut config = crate::config::AppConfig::default();
    /// assert!(!config.has_credentials());
    /// config.refresh_token = Some("token".to_string());
    /// assert!(config.has_credentials());
    /// ```
    pub fn has_credentials(&self) -> bool {
        match self.auth_method {
            AuthMethod::RefreshToken => self.refresh_token.as_ref().is_some_and(|t| !t.is_empty()),
            AuthMethod::CredsFile => self.kiro_creds_file.as_ref().is_some_and(|f| !f.is_empty()),
            AuthMethod::CliDb => self
                .kiro_cli_db_file
                .as_ref()
                .is_some_and(|d| !d.is_empty()),
        }
    }

    /// Returns the host to use when the GUI itself needs to *connect to*
    /// the embedded gateway (health checks, model list, usage, etc.),
    /// rewriting the wildcard bind address `0.0.0.0` to the loopback
    /// address `127.0.0.1` since `0.0.0.0` is a valid bind address but not
    /// a valid address to open a client connection to.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let config = crate::config::AppConfig { server_host: "0.0.0.0".to_string(), ..Default::default() };
    /// assert_eq!(config.probe_host(), "127.0.0.1");
    /// ```
    pub fn probe_host(&self) -> &str {
        if self.server_host == "0.0.0.0" {
            "127.0.0.1"
        } else {
            &self.server_host
        }
    }
}

/// Generates a new random `sk-`-prefixed API key (35 characters total: the
/// `sk-` prefix plus 32 alphanumeric characters), used both for the initial
/// key created on first run and for the "Generate" button in the settings
/// UI.
///
/// Randomness is drawn from `uuid::Uuid::new_v4()` (a cryptographically
/// unrelated but sufficiently random source for this purpose) rather than a
/// dedicated CSPRNG, mapping each random byte into the allowed alphanumeric
/// character set.
///
/// # Examples
///
/// ```ignore
/// let key = crate::config::generate_api_key();
/// assert!(key.starts_with("sk-"));
/// assert_eq!(key.len(), 35);
/// ```
pub fn generate_api_key() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(35);
    out.push_str("sk-");
    while out.len() < 35 {
        for byte in uuid::Uuid::new_v4().as_bytes() {
            if out.len() >= 35 {
                break;
            }
            out.push(CHARS[(*byte as usize) % CHARS.len()] as char);
        }
    }
    out
}

/// Resolves the platform-appropriate application data directory
/// (`dirs::data_dir()/lanius`) under which the config file, credentials
/// cache, and debug logs are stored. Does not create the directory; callers
/// that need it to exist (e.g. [`save_config_to`]) create it on demand.
///
/// # Examples
///
/// ```ignore
/// let data_dir = crate::config::get_app_data_dir()?;
/// let debug_dir = data_dir.join("debug_logs");
/// ```
pub fn get_app_data_dir() -> Result<PathBuf, String> {
    Ok(dirs::data_dir()
        .ok_or("Failed to get app data directory")?
        .join("lanius"))
}

fn get_config_path() -> Result<PathBuf, String> {
    Ok(get_app_data_dir()?.join("config.json"))
}

/// Loads the application's configuration from its standard on-disk
/// location, or returns [`AppConfig::default`] if no config file exists
/// yet (first run). A config file that exists but fails to parse is
/// reported as an error rather than silently discarded, so a corrupted
/// config is never overwritten with defaults without the caller knowing.
///
/// # Examples
///
/// ```ignore
/// let config = crate::config::load_config().await?;
/// println!("gateway port: {}", config.server_port);
/// ```
pub async fn load_config() -> Result<AppConfig, String> {
    load_config_from(&get_config_path()?).await
}

pub(crate) async fn load_config_from(config_path: &std::path::Path) -> Result<AppConfig, String> {
    if !config_path.exists() {
        return Ok(AppConfig::default());
    }

    let content = fs::read_to_string(config_path)
        .await
        .map_err(|e| format!("Failed to read config file: {}", e))?;

    let config: AppConfig = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse config file: {}", e))?;

    Ok(config)
}

/// Persists `config` to its standard on-disk location as pretty-printed
/// JSON, creating the parent application data directory if it does not yet
/// exist. This performs real filesystem writes and should be called from an
/// async context (it uses `tokio::fs`), never assumed to be instantaneous.
///
/// # Examples
///
/// ```ignore
/// let mut config = crate::config::load_config().await?;
/// config.server_port = 9000;
/// crate::config::save_config(&config).await?;
/// ```
pub async fn save_config(config: &AppConfig) -> Result<(), String> {
    save_config_to(&get_config_path()?, config).await
}

pub(crate) async fn save_config_to(
    config_path: &std::path::Path,
    config: &AppConfig,
) -> Result<(), String> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    let content = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;

    fs::write(config_path, content)
        .await
        .map_err(|e| format!("Failed to write config file: {}", e))?;

    Ok(())
}

/// Result of scanning well-known filesystem locations for existing Kiro
/// credential files (`kiro-credentials.json`) or CLI SQLite databases, used
/// to suggest (or auto-select, on first run) an authentication method
/// without requiring the user to manually locate their credentials.
#[derive(Debug, Clone, Default)]
pub struct CredentialScan {
    pub creds_files: Vec<String>,
    pub cli_dbs: Vec<String>,
    pub recommended_method: Option<AuthMethod>,
    pub recommended_path: Option<String>,
}

/// Scans a fixed list of platform-conventional paths for Kiro credential
/// files and CLI SQLite databases (e.g. under `~/.aws/sso/cache`,
/// `~/.config/kiro`, `~/.kiro-cli`, etc.) and returns whichever exist,
/// along with a recommendation for which one to use.
///
/// This only reads filesystem *metadata* (existence checks via
/// `Path::exists`), never the contents of any credential file. CLI SQLite
/// databases are preferred over credential files when both exist, since a
/// CLI login is a stronger signal of an active, already-authenticated
/// session — reflected in [`recommended_method`](CredentialScan::recommended_method)
/// checking `cli_dbs` first.
///
/// # Examples
///
/// ```ignore
/// let scan = crate::config::scan_all_credentials()?;
/// if let (Some(method), Some(path)) = (scan.recommended_method, scan.recommended_path) {
///     println!("suggest {} at {path}", method.as_str());
/// }
/// ```
pub fn scan_all_credentials() -> Result<CredentialScan, String> {
    let home = dirs::home_dir().ok_or("Failed to get home directory")?;
    let data_dir = dirs::data_local_dir().ok_or("Failed to get data directory")?;

    let creds_files = existing_paths([
        home.join(".aws/sso/cache/kiro-auth-token.json"),
        home.join(".config/kiro/kiro-credentials.json"),
        home.join(".kiro/kiro-credentials.json"),
        home.join("kiro-credentials.json"),
        PathBuf::from("/etc/kiro/kiro-credentials.json"),
    ]);

    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut cli_db_candidates = vec![
        data_dir.join("kiro-cli/data.sqlite3"),
        home.join(".local/share/kiro-cli/data.sqlite3"),
        home.join(".config/kiro-cli/data.sqlite3"),
        home.join(".kiro-cli/data.sqlite3"),
    ];
    // `data_local_dir()` above is `%LOCALAPPDATA%` on Windows; also check
    // the roaming `%APPDATA%` in case kiro-cli stores its database there.
    #[cfg(target_os = "windows")]
    if let Some(roaming) = dirs::data_dir() {
        cli_db_candidates.push(roaming.join("kiro-cli").join("data.sqlite3"));
    }
    let cli_dbs = existing_paths(cli_db_candidates);

    let (recommended_method, recommended_path) = if let Some(first) = cli_dbs.first() {
        (Some(AuthMethod::CliDb), Some(first.clone()))
    } else if let Some(first) = creds_files.first() {
        (Some(AuthMethod::CredsFile), Some(first.clone()))
    } else {
        (None, None)
    };

    Ok(CredentialScan {
        creds_files,
        cli_dbs,
        recommended_method,
        recommended_path,
    })
}

/// Filters `candidates` down to only the paths that actually exist on disk,
/// converting each surviving `PathBuf` to a UTF-8 `String` (silently
/// dropping any path that is not valid UTF-8, which should not occur for
/// the fixed ASCII candidate paths this is used with).
fn existing_paths(candidates: impl IntoIterator<Item = PathBuf>) -> Vec<String> {
    candidates
        .into_iter()
        .filter(|p| p.exists())
        .filter_map(|p| p.to_str().map(str::to_string))
        .collect()
}

/// Fills in required-but-currently-unset fields on `config` in place:
/// generates a stable `client_id` and a `proxy_api_key` if either is
/// missing/empty, and — if no credential is configured at all — attempts to
/// auto-select one via [`scan_all_credentials`]. Returns `true` if any
/// field was actually changed, so the caller (`controller.rs`'s bootstrap
/// path) knows whether it needs to persist the config back to disk.
///
/// This never overwrites an existing credential the user has already
/// configured; auto-discovery only kicks in when `refresh_token`,
/// `kiro_creds_file`, and `kiro_cli_db_file` are all empty/absent. A
/// credential-scan failure (e.g. home directory undeterminable) is logged
/// as a warning and treated as "nothing found" rather than propagated,
/// since bootstrap must be able to proceed even without auto-discovered
/// credentials.
///
/// # Examples
///
/// ```ignore
/// let mut config = crate::config::load_config().await?;
/// if crate::config::ensure_defaults(&mut config) {
///     crate::config::save_config(&config).await?;
/// }
/// ```
pub fn ensure_defaults(config: &mut AppConfig) -> bool {
    let mut changed = false;

    if config.client_id.as_ref().is_none_or(|id| id.is_empty()) {
        config.client_id = Some(uuid::Uuid::new_v4().to_string());
        changed = true;
    }

    if config.proxy_api_key.is_empty() {
        config.proxy_api_key = generate_api_key();
        changed = true;
    }

    let has_credential = config.refresh_token.as_ref().is_some_and(|v| !v.is_empty())
        || config
            .kiro_creds_file
            .as_ref()
            .is_some_and(|v| !v.is_empty())
        || config
            .kiro_cli_db_file
            .as_ref()
            .is_some_and(|v| !v.is_empty());

    if !has_credential {
        match scan_all_credentials() {
            Ok(scan) => {
                if let (Some(method), Some(path)) = (scan.recommended_method, scan.recommended_path)
                {
                    config.auth_method = method;
                    match method {
                        AuthMethod::CliDb => config.kiro_cli_db_file = Some(path),
                        _ => config.kiro_creds_file = Some(path),
                    }
                    changed = true;
                }
            }
            Err(e) => tracing::warn!("credential auto-scan failed: {e}"),
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_have_the_documented_shape() {
        let key = generate_api_key();
        assert!(
            key.starts_with("sk-"),
            "key must keep the sk- prefix: {key}"
        );
        assert_eq!(key.len(), 35, "sk- plus 32 characters");
        assert!(key[3..].chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(key, generate_api_key(), "keys must not repeat");
    }

    #[test]
    fn credentials_are_required_per_auth_method() {
        let mut cfg = AppConfig::default();
        assert!(!cfg.has_credentials(), "default config has no credentials");

        cfg.refresh_token = Some(String::new());
        assert!(!cfg.has_credentials(), "empty string is not a credential");

        cfg.refresh_token = Some("tok".into());
        assert!(cfg.has_credentials());

        cfg.auth_method = AuthMethod::CliDb;
        assert!(!cfg.has_credentials());
        cfg.kiro_cli_db_file = Some("/tmp/data.sqlite3".into());
        assert!(cfg.has_credentials());
    }

    #[test]
    fn probe_host_rewrites_the_wildcard_bind_address() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.probe_host(), "127.0.0.1");
        cfg.server_host = "0.0.0.0".into();
        assert_eq!(
            cfg.probe_host(),
            "127.0.0.1",
            "0.0.0.0 cannot be used as a connect address"
        );
        cfg.server_host = "192.168.1.10".into();
        assert_eq!(cfg.probe_host(), "192.168.1.10");
    }

    #[test]
    fn ensure_defaults_fills_identity_and_key_once() {
        let mut cfg = AppConfig::default();
        assert!(ensure_defaults(&mut cfg), "first call must report changes");
        let client_id = cfg.client_id.clone().expect("client id generated");
        let key = cfg.proxy_api_key.clone();
        assert!(!client_id.is_empty());
        assert!(key.starts_with("sk-"));

        ensure_defaults(&mut cfg);
        assert_eq!(cfg.client_id.as_deref(), Some(client_id.as_str()));
        assert_eq!(cfg.proxy_api_key, key);
    }

    // The legacy fixture below still carries the removed `fake_reasoning*`
    // keys: configs saved by older builds must keep loading (unknown keys
    // are ignored).
    #[test]
    fn old_config_files_without_the_new_fields_still_parse() {
        let json = r#"{
            "auth_method": "cli_db",
            "refresh_token": null,
            "kiro_creds_file": null,
            "kiro_cli_db_file": "/tmp/data.sqlite3",
            "proxy_api_key": "sk-old",
            "server_host": "127.0.0.1",
            "server_port": 8123,
            "kiro_region": "us-east-1",
            "vpn_proxy_url": null,
            "first_token_timeout": 15.0,
            "streaming_read_timeout": 300.0,
            "fake_reasoning": true,
            "fake_reasoning_max_tokens": 4000,
            "truncation_recovery": true,
            "log_level": "INFO",
            "debug_mode": "off"
        }"#;
        let cfg: AppConfig = serde_json::from_str(json).expect("legacy config must parse");
        assert_eq!(cfg.auth_method, AuthMethod::CliDb);
        assert_eq!(cfg.server_port, 8123);
        assert_eq!(cfg.language, None);
    }

    #[tokio::test]
    async fn missing_file_yields_defaults_but_a_corrupt_file_is_an_error() {
        let dir = std::env::temp_dir().join(format!("lanius-cfg-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.json");

        let loaded = load_config_from(&path)
            .await
            .expect("missing file is not an error");
        assert_eq!(loaded.server_port, 18000);

        let saved = AppConfig {
            server_port: 9999,
            auto_start_server: true,
            proxy_api_key: "sk-keepme".into(),
            ..Default::default()
        };
        save_config_to(&path, &saved)
            .await
            .expect("save must succeed");
        let reloaded = load_config_from(&path)
            .await
            .expect("valid file must parse");
        assert_eq!(reloaded, saved);

        tokio::fs::write(&path, b"{ not json").await.unwrap();
        let err = load_config_from(&path)
            .await
            .expect_err("corrupt file must error");
        assert!(err.contains("parse"), "unexpected error: {err}");

        tokio::fs::write(&path, b"{\"server_port\": 1234}")
            .await
            .unwrap();
        assert!(
            load_config_from(&path).await.is_err(),
            "a partially written config must not be accepted as complete"
        );

        let on_disk = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(on_disk, "{\"server_port\": 1234}");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn auth_method_round_trips_through_the_ui_string() {
        for method in [
            AuthMethod::RefreshToken,
            AuthMethod::CredsFile,
            AuthMethod::CliDb,
        ] {
            assert_eq!(AuthMethod::from_str_or_default(method.as_str()), method);
        }
        assert_eq!(
            AuthMethod::from_str_or_default("nonsense"),
            AuthMethod::RefreshToken,
            "unknown values must fall back, never panic"
        );
    }
}
