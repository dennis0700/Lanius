//! Environment-variable-driven configuration for the gateway.
//!
//! This module defines [`Config`] along with the hardcoded defaults, URL templates, and constants used
//! throughout `lanius-core`. [`Config::from_env`] is the normal entry
//! point: it starts from [`Config::default`] and overlays any recognized
//! environment variables, falling back to defaults on missing or
//! unparseable values. [`server`](crate::server) constructs a [`Config`] at
//! startup and threads it through [`upstream`](crate::upstream),
//! [`auth`](crate::auth), and the API route handlers in [`api`](crate::api).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// URL template (with a `{region}` placeholder) for refreshing a Kiro desktop
/// OAuth token.
pub const KIRO_REFRESH_URL_TEMPLATE: &str =
    "https://prod.{region}.auth.desktop.kiro.dev/refreshToken";

/// URL template (with a `{region}` placeholder) for the AWS SSO OIDC token
/// endpoint, used for IAM Identity Center-based accounts.
pub const AWS_SSO_OIDC_URL_TEMPLATE: &str = "https://oidc.{region}.amazonaws.com/token";

/// URL template (with a `{region}` placeholder) for the Kiro runtime API host
/// used to send chat/completion requests.
pub const KIRO_API_HOST_TEMPLATE: &str = "https://runtime.{region}.kiro.dev";

/// URL template (with a `{region}` placeholder) for the Kiro control-plane
/// host used for model listing and account/usage queries.
pub const KIRO_Q_HOST_TEMPLATE: &str = "https://runtime.{region}.kiro.dev";

/// How far ahead of expiry an access token is proactively refreshed.
pub const TOKEN_REFRESH_THRESHOLD: Duration = Duration::from_secs(600);

/// Maximum number of attempts for non-streaming upstream requests
/// (see [`crate::upstream::KiroHttpClient`]).
pub const MAX_RETRIES: u32 = 3;

/// Base delay for the exponential backoff applied between retried upstream
/// requests; actual delay is `BASE_RETRY_DELAY * 2^attempt`.
pub const BASE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How long a fetched model catalog is considered fresh before
/// [`crate::model::ModelInfoCache::is_stale`] reports it as stale.
pub const MODEL_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Fallback maximum input token count used when a model's real limit is
/// unknown or not yet cached.
pub const DEFAULT_MAX_INPUT_TOKENS: u32 = 200_000;

/// Crate version, taken from `Cargo.toml` at compile time.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Human-readable application title, used in server metadata/UI.
pub const APP_TITLE: &str = "Lanius";
/// One-line description of the gateway's purpose, used in server metadata/UI.
pub const APP_DESCRIPTION: &str = "OpenAI- and Anthropic-compatible bridge for the Kiro API (Amazon Q Developer / AWS CodeWhisperer).";

/// Default set of model ids to hide from the advertised `/v1/models` list
/// (they remain resolvable by name, just not listed).
///
/// # Examples
///
/// ```
/// use lanius_core::config::default_hidden_from_list;
///
/// assert_eq!(default_hidden_from_list(), vec!["auto".to_string()]);
/// ```
pub fn default_hidden_from_list() -> Vec<String> {
    vec!["auto".to_string()]
}

/// Default mapping of externally-facing alias names to internal model ids,
/// applied before normalization in [`crate::model::ModelResolver`].
///
/// # Examples
///
/// ```
/// use lanius_core::config::default_model_aliases;
///
/// let aliases = default_model_aliases();
/// assert_eq!(aliases.get("auto-kiro"), Some(&"auto".to_string()));
/// ```
pub fn default_model_aliases() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("auto-kiro".to_string(), "auto".to_string());
    m
}

/// Built-in snapshot of the Kiro model catalog, served when the live catalog
/// cannot be fetched from upstream (see
/// [`crate::model::ModelInfoCache::load_fallback`]).
///
/// # Examples
///
/// ```
/// use lanius_core::config::fallback_models;
///
/// let models = fallback_models();
/// assert!(models.contains(&"auto"));
/// ```
pub fn fallback_models() -> Vec<&'static str> {
    vec![
        "auto",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-opus-4.8",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "claude-opus-4.7",
        "claude-opus-4.6",
        "claude-sonnet-4.6",
        "claude-opus-4.5",
        "claude-sonnet-4.5",
        "claude-sonnet-4",
        "claude-haiku-4.5",
        "deepseek-3.2",
        "minimax-m2.5",
        "minimax-m2.1",
        "glm-5",
        "qwen3-coder-next",
    ]
}

/// Default bind host when `SERVER_HOST` is not set.
pub const DEFAULT_SERVER_HOST: &str = "0.0.0.0";
/// Default bind port when `SERVER_PORT` is not set.
pub const DEFAULT_SERVER_PORT: u16 = 18000;

/// Controls how much internal debug logging/dumping the gateway performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DebugMode {
    /// No debug logging beyond normal tracing.
    #[default]
    Off,
    /// Log/dump extra detail only when errors occur.
    Errors,
    /// Log/dump extra detail for every request.
    All,
}

impl DebugMode {
    /// Returns `true` for any mode other than [`DebugMode::Off`].
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::config::DebugMode;
    ///
    /// assert!(!DebugMode::Off.is_enabled());
    /// assert!(DebugMode::All.is_enabled());
    /// ```
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Parses a `DEBUG_MODE` environment value case-insensitively, defaulting
    /// to [`DebugMode::Off`] for anything unrecognized.
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "all" => Self::All,
            "errors" => Self::Errors,
            _ => Self::Off,
        }
    }
}

/// Top-level gateway configuration, populated via [`Config::from_env`] (or
/// [`Config::default`] in tests) and shared read-only across the server via
/// `Arc<Config>` (see [`crate::server::AppState`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Address the HTTP server binds to.
    pub server_host: String,
    /// Port the HTTP server binds to.
    pub server_port: u16,
    /// Shared-secret API key that clients must present as a Bearer token to
    /// use this gateway.
    pub proxy_api_key: String,

    /// Optional HTTP/HTTPS/SOCKS5 proxy URL used for all upstream Kiro requests.
    pub vpn_proxy_url: Option<String>,

    /// Refresh token used to authenticate with Kiro.
    pub refresh_token: Option<String>,
    /// Optional AWS IAM profile ARN associated with the refresh token.
    pub profile_arn: Option<String>,
    /// AWS region used to build the Kiro/OIDC URL templates.
    pub region: String,
    /// Optional path to a Kiro desktop credentials file to read tokens from.
    pub kiro_creds_file: Option<PathBuf>,
    /// Optional path to the Kiro CLI's SQLite database, an alternate
    /// credential source.
    pub kiro_cli_db_file: Option<PathBuf>,
    /// Whether the SQLite credentials database is opened read-only.
    pub sqlite_readonly: bool,

    /// Maximum length of a tool description before it is truncated when
    /// forwarded to Kiro.
    pub tool_description_max_length: usize,
    /// Whether truncated tool/content output triggers the recovery-prompt
    /// flow in [`crate::truncation`].
    pub truncation_recovery: bool,

    /// How long to wait for the first byte of a streaming response before
    /// giving up with [`crate::error::GatewayError::FirstTokenTimeout`].
    pub first_token_timeout: Duration,
    /// How long to wait between subsequent chunks of a streaming response
    /// before giving up with [`crate::error::GatewayError::StreamReadTimeout`].
    pub streaming_read_timeout: Duration,
    /// Maximum number of attempts made while waiting for the first streamed
    /// token (see [`crate::upstream::KiroHttpClient`]).
    pub first_token_max_retries: u32,

    /// Maximum request payload size, in bytes, sent to Kiro.
    pub kiro_max_payload_bytes: usize,
    /// Whether oversized payloads are automatically trimmed instead of
    /// rejected.
    pub auto_trim_payload: bool,

    /// Whether the web-search tool capability is advertised/enabled.
    pub web_search_enabled: bool,
    /// Log verbosity level (e.g. `"INFO"`, `"DEBUG"`).
    pub log_level: String,
    /// Extra internal debug logging/dumping mode.
    pub debug_mode: DebugMode,
    /// Directory debug dumps are written to when `debug_mode` is enabled.
    pub debug_dir: PathBuf,

    /// Externally-facing alias names mapped to internal model ids, consulted
    /// before normalization in [`crate::model::ModelResolver`].
    pub model_aliases: HashMap<String, String>,
    /// Model ids that remain resolvable but are hidden from the advertised
    /// `/v1/models` list.
    pub hidden_from_list: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_host: DEFAULT_SERVER_HOST.to_string(),
            server_port: DEFAULT_SERVER_PORT,
            proxy_api_key: "my-super-secret-password-123".to_string(),
            vpn_proxy_url: None,
            refresh_token: None,
            profile_arn: None,
            region: "us-east-1".to_string(),
            kiro_creds_file: None,
            kiro_cli_db_file: None,
            sqlite_readonly: false,
            tool_description_max_length: 10_000,
            truncation_recovery: true,
            first_token_timeout: Duration::from_secs_f64(15.0),
            streaming_read_timeout: Duration::from_secs_f64(300.0),
            first_token_max_retries: 3,
            kiro_max_payload_bytes: 600_000,
            auto_trim_payload: false,
            web_search_enabled: true,
            log_level: "INFO".to_string(),
            debug_mode: DebugMode::Off,
            debug_dir: PathBuf::from("debug_logs"),
            model_aliases: default_model_aliases(),
            hidden_from_list: default_hidden_from_list(),
        }
    }
}

impl Config {
    /// Builds a [`Config`] by reading environment variables, falling back to
    /// [`Config::default`] for anything unset or unparseable. This is the
    /// standard way the gateway is configured in production; see
    /// [`crate::server::serve`]/[`crate::server::spawn`].
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::Config;
    ///
    /// // Reads recognized environment variables, falling back to defaults for
    /// // anything unset.
    /// let cfg = Config::from_env();
    /// assert!(!cfg.region.is_empty());
    /// ```
    pub fn from_env() -> Self {
        let d = Self::default();

        Self {
            server_host: env_string("SERVER_HOST").unwrap_or(d.server_host),
            server_port: env_parse("SERVER_PORT").unwrap_or(d.server_port),
            proxy_api_key: env_string("PROXY_API_KEY").unwrap_or(d.proxy_api_key),

            vpn_proxy_url: env_string("VPN_PROXY_URL"),

            refresh_token: env_string("REFRESH_TOKEN"),
            profile_arn: env_string("PROFILE_ARN"),
            region: env_string("KIRO_REGION").unwrap_or(d.region),
            kiro_creds_file: env_string("KIRO_CREDS_FILE").map(expand_tilde),
            kiro_cli_db_file: env_string("KIRO_CLI_DB_FILE").map(expand_tilde),
            sqlite_readonly: env_bool("SQLITE_READONLY", false),

            tool_description_max_length: env_parse("TOOL_DESCRIPTION_MAX_LENGTH")
                .unwrap_or(d.tool_description_max_length),
            truncation_recovery: env_bool("TRUNCATION_RECOVERY", true),

            first_token_timeout: env_parse::<f64>("FIRST_TOKEN_TIMEOUT")
                .map(Duration::from_secs_f64)
                .unwrap_or(d.first_token_timeout),
            streaming_read_timeout: env_parse::<f64>("STREAMING_READ_TIMEOUT")
                .map(Duration::from_secs_f64)
                .unwrap_or(d.streaming_read_timeout),
            first_token_max_retries: env_parse("FIRST_TOKEN_MAX_RETRIES")
                .unwrap_or(d.first_token_max_retries),

            kiro_max_payload_bytes: env_parse("KIRO_MAX_PAYLOAD_BYTES")
                .unwrap_or(d.kiro_max_payload_bytes),
            auto_trim_payload: env_bool("AUTO_TRIM_PAYLOAD", false),

            web_search_enabled: env_bool("WEB_SEARCH_ENABLED", true),
            log_level: env_string("LOG_LEVEL")
                .map(|s| s.to_ascii_uppercase())
                .unwrap_or(d.log_level),
            debug_mode: DebugMode::parse(&env_string("DEBUG_MODE").unwrap_or_default()),
            debug_dir: env_string("DEBUG_DIR")
                .map(PathBuf::from)
                .unwrap_or(d.debug_dir),

            model_aliases: d.model_aliases,
            hidden_from_list: d.hidden_from_list,
        }
    }

    /// Renders [`KIRO_REFRESH_URL_TEMPLATE`] for this config's region.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::Config;
    ///
    /// let cfg = Config { region: "eu-central-1".into(), ..Default::default() };
    /// assert_eq!(
    ///     cfg.refresh_url(),
    ///     "https://prod.eu-central-1.auth.desktop.kiro.dev/refreshToken"
    /// );
    /// ```
    pub fn refresh_url(&self) -> String {
        KIRO_REFRESH_URL_TEMPLATE.replace("{region}", &self.region)
    }

    /// Renders [`AWS_SSO_OIDC_URL_TEMPLATE`] for this config's region.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::Config;
    ///
    /// let cfg = Config { region: "eu-central-1".into(), ..Default::default() };
    /// assert_eq!(cfg.oidc_url(), "https://oidc.eu-central-1.amazonaws.com/token");
    /// ```
    pub fn oidc_url(&self) -> String {
        AWS_SSO_OIDC_URL_TEMPLATE.replace("{region}", &self.region)
    }

    /// Renders [`KIRO_API_HOST_TEMPLATE`] for this config's region.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::Config;
    ///
    /// let cfg = Config { region: "eu-central-1".into(), ..Default::default() };
    /// assert_eq!(cfg.api_host(), "https://runtime.eu-central-1.kiro.dev");
    /// ```
    pub fn api_host(&self) -> String {
        KIRO_API_HOST_TEMPLATE.replace("{region}", &self.region)
    }

    /// Renders [`KIRO_Q_HOST_TEMPLATE`] for this config's region.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::Config;
    ///
    /// let cfg = Config { region: "eu-central-1".into(), ..Default::default() };
    /// assert_eq!(cfg.q_host(), "https://runtime.eu-central-1.kiro.dev");
    /// ```
    pub fn q_host(&self) -> String {
        KIRO_Q_HOST_TEMPLATE.replace("{region}", &self.region)
    }

    /// Full URL of the Kiro chat/completion endpoint used to send converted
    /// requests upstream.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::Config;
    ///
    /// let cfg = Config { region: "eu-central-1".into(), ..Default::default() };
    /// assert_eq!(
    ///     cfg.generate_assistant_response_url(),
    ///     "https://runtime.eu-central-1.kiro.dev/generateAssistantResponse"
    /// );
    /// ```
    pub fn generate_assistant_response_url(&self) -> String {
        format!("{}/generateAssistantResponse", self.api_host())
    }

    /// Full URL of the Kiro model-catalog listing endpoint.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::Config;
    ///
    /// let cfg = Config { region: "eu-central-1".into(), ..Default::default() };
    /// assert_eq!(
    ///     cfg.list_available_models_url(),
    ///     "https://runtime.eu-central-1.kiro.dev/ListAvailableModels"
    /// );
    /// ```
    pub fn list_available_models_url(&self) -> String {
        format!("{}/ListAvailableModels", self.q_host())
    }

    /// Validates invariants that must hold before the server starts serving
    /// traffic (non-empty proxy key/region, non-zero port, well-formed proxy
    /// URL scheme). Returns a [`crate::error::GatewayError::Config`] on the
    /// first violation found.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::Config;
    ///
    /// assert!(Config::default().validate().is_ok());
    ///
    /// let bad = Config { proxy_api_key: "  ".into(), ..Default::default() };
    /// assert!(bad.validate().is_err());
    /// ```
    pub fn validate(&self) -> crate::error::Result<()> {
        use crate::error::GatewayError;

        if self.proxy_api_key.trim().is_empty() {
            return Err(GatewayError::Config(
                "PROXY_API_KEY must not be empty; clients could otherwise authenticate with any key"
                    .into(),
            ));
        }
        if self.region.trim().is_empty() {
            return Err(GatewayError::Config("KIRO_REGION must not be empty".into()));
        }
        if self.server_port == 0 {
            return Err(GatewayError::Config("SERVER_PORT must not be 0".into()));
        }
        if let Some(url) = &self.vpn_proxy_url {
            if !(url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with("socks5://")
                || url.starts_with("socks5h://"))
            {
                return Err(GatewayError::Config(format!(
                    "VPN_PROXY_URL must start with http://, https://, socks5:// or socks5h:// (got {url:?})"
                )));
            }
        }
        Ok(())
    }
}

fn env_string(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    let raw = env_string(key)?;
    match raw.parse::<T>() {
        Ok(v) => Some(v),
        Err(_) => {
            // Malformed values fall back to the caller's default rather than
            // failing startup; we still warn so the misconfiguration is visible.
            tracing::warn!(
                key,
                value = %raw,
                "unparseable configuration value; falling back to default"
            );
            None
        }
    }
}

fn env_bool(key: &str, default: bool) -> bool {
    match env_string(key) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        None => default,
    }
}

// Expands a leading "~" or "~/" to the user's home directory, matching shell
// tilde-expansion behavior for path-valued environment variables.
fn expand_tilde(raw: String) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_templates_render_region() {
        let cfg = Config {
            region: "eu-central-1".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.refresh_url(),
            "https://prod.eu-central-1.auth.desktop.kiro.dev/refreshToken"
        );
        assert_eq!(
            cfg.oidc_url(),
            "https://oidc.eu-central-1.amazonaws.com/token"
        );
        assert_eq!(cfg.api_host(), "https://runtime.eu-central-1.kiro.dev");
        assert_eq!(
            cfg.generate_assistant_response_url(),
            "https://runtime.eu-central-1.kiro.dev/generateAssistantResponse"
        );
    }

    #[test]
    fn defaults_match_documented_values() {
        let c = Config::default();
        assert_eq!(c.server_host, "0.0.0.0");
        assert_eq!(c.server_port, 18000);
        assert_eq!(c.region, "us-east-1");
        assert_eq!(c.tool_description_max_length, 10_000);
        assert_eq!(c.kiro_max_payload_bytes, 600_000);
        assert_eq!(c.first_token_timeout, Duration::from_secs(15));
        assert_eq!(c.streaming_read_timeout, Duration::from_secs(300));
        assert_eq!(c.first_token_max_retries, 3);
        assert!(c.truncation_recovery);
        assert!(c.web_search_enabled);
        assert!(!c.auto_trim_payload);
        assert!(!c.sqlite_readonly);
        assert_eq!(c.debug_mode, DebugMode::Off);
    }

    #[test]
    fn debug_mode_parsing() {
        assert_eq!(DebugMode::parse("all"), DebugMode::All);
        assert_eq!(DebugMode::parse("ALL"), DebugMode::All);
        assert_eq!(DebugMode::parse("errors"), DebugMode::Errors);
        assert_eq!(DebugMode::parse(""), DebugMode::Off);
        assert_eq!(DebugMode::parse("nonsense"), DebugMode::Off);
        assert!(!DebugMode::Off.is_enabled());
        assert!(DebugMode::Errors.is_enabled());
    }

    #[test]
    fn validate_rejects_empty_api_key() {
        let cfg = Config {
            proxy_api_key: "   ".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_proxy_scheme() {
        let cfg = Config {
            vpn_proxy_url: Some("ftp://nope".into()),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());

        for ok in [
            "http://p:8080",
            "https://p:8080",
            "socks5://p:1080",
            "socks5h://p:1080",
        ] {
            let cfg = Config {
                vpn_proxy_url: Some(ok.into()),
                ..Default::default()
            };
            assert!(cfg.validate().is_ok(), "{ok} should be accepted");
        }
    }

    #[test]
    fn validate_accepts_default() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn fallback_catalog_matches_the_verified_upstream_snapshot() {
        let models = fallback_models();

        let expected = [
            "auto",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4.8",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "claude-opus-4.7",
            "claude-opus-4.6",
            "claude-sonnet-4.6",
            "claude-opus-4.5",
            "claude-sonnet-4.5",
            "claude-sonnet-4",
            "claude-haiku-4.5",
            "deepseek-3.2",
            "minimax-m2.5",
            "minimax-m2.1",
            "glm-5",
            "qwen3-coder-next",
        ];
        assert_eq!(
            models, expected,
            "fallback catalog drifted from the snapshot"
        );

        for id in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4.8",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            assert!(models.contains(&id), "{id} must be advertised");
        }

        let unique: std::collections::HashSet<_> = models.iter().collect();
        assert_eq!(unique.len(), models.len(), "duplicate model id");
        assert_eq!(models.first(), Some(&"auto"));
    }
}
