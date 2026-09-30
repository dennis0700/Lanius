//! Unified error type for the gateway, plus Kiro- and network-error
//! classification/enhancement.
//!
//! [`GatewayError`] is the single error type threaded through
//! [`crate::upstream`], [`crate::auth`], and the API route handlers in
//! [`crate::api`]; [`crate::server`] maps it to an HTTP status and JSON
//! body via [`GatewayError::http_status`] and [`GatewayError::user_message`].
//! This module also classifies raw Kiro error payloads
//! ([`enhance_kiro_error`]) and raw `reqwest` transport failures
//! ([`classify_network_error`]) into actionable, user-facing information.

use std::fmt;

use serde::Serialize;
use serde_json::Value;

/// The single error type used across the gateway, covering client-facing
/// request errors, Kiro upstream failures, transport-level network errors,
/// timeouts, and internal/config errors.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// The caller's proxy API key was missing or invalid.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The incoming request was malformed or failed validation before being
    /// sent upstream.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Authenticating with Kiro/AWS (token refresh, OIDC exchange) failed.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Kiro returned a non-2xx response; `info` carries the classified,
    /// user-facing explanation produced by [`enhance_kiro_error`].
    #[error("upstream error {status}: {info}")]
    Upstream {
        /// HTTP status code Kiro returned.
        status: u16,
        /// Classified, user-facing explanation of the error.
        info: Box<KiroErrorInfo>,
    },

    /// A transport-level failure occurred talking to Kiro; `info` is the
    /// classification produced by [`classify_network_error`].
    #[error("network error: {0}")]
    Network(Box<NetworkErrorInfo>),

    /// No bytes were received from Kiro within the configured
    /// `first_token_timeout` (see [`crate::config::Config`]).
    #[error("first token timeout after {0:?}")]
    FirstTokenTimeout(std::time::Duration),

    /// The stream stalled for longer than the configured
    /// `streaming_read_timeout` after the first token was received.
    #[error("streaming read timeout after {0:?}")]
    StreamReadTimeout(std::time::Duration),

    /// The requested model id could not be resolved to a known/valid model.
    #[error("unknown model: {0}")]
    UnknownModel(String),

    /// The gateway's own configuration is invalid (see [`crate::config::Config::validate`]).
    #[error("configuration error: {0}")]
    Config(String),

    /// A filesystem or other I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// A local SQLite operation (credentials/state store) failed.
    #[error("sqlite error: {0}")]
    Sqlite(String),

    /// Any other unexpected internal failure.
    #[error("internal error: {0}")]
    Internal(String),
}

impl GatewayError {
    /// Maps this error to the HTTP status code it should be reported to
    /// clients as. Network errors use the status code suggested by their
    /// [`NetworkErrorInfo::suggested_http_code`]; everything else uses a
    /// fixed mapping (e.g. auth failures -> 401, timeouts -> 504).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::GatewayError;
    ///
    /// let err = GatewayError::Unauthorized("missing key".into());
    /// assert_eq!(err.http_status(), 401);
    /// ```
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Unauthorized(_) => 401,
            Self::InvalidRequest(_) => 400,
            Self::Auth(_) => 401,
            Self::Upstream { status, .. } => *status,
            Self::Network(info) => info.suggested_http_code,
            Self::FirstTokenTimeout(_) | Self::StreamReadTimeout(_) => 504,
            Self::UnknownModel(_) => 400,
            Self::Config(_) => 500,
            Self::Io(_) | Self::Json(_) | Self::Sqlite(_) | Self::Internal(_) => 500,
        }
    }

    /// Returns the message that is safe and helpful to show to the end user.
    /// For [`GatewayError::Upstream`] and [`GatewayError::Network`] this is
    /// the classified, human-friendly message rather than raw upstream/error
    /// text (which may contain sensitive details); other variants use their
    /// `Display` implementation.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::GatewayError;
    ///
    /// let err = GatewayError::UnknownModel("gpt-5".into());
    /// assert_eq!(err.user_message(), "unknown model: gpt-5");
    /// ```
    pub fn user_message(&self) -> String {
        match self {
            Self::Upstream { info, .. } => info.user_message.clone(),
            Self::Network(info) => info.user_message.clone(),
            other => other.to_string(),
        }
    }

    /// Returns an operator-facing description of this error for logs. Unlike
    /// [`Self::user_message`], this keeps diagnostic detail such as Kiro's
    /// raw error message/reason and the transport error chain.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::GatewayError;
    ///
    /// let err = GatewayError::Internal("boom".into());
    /// assert_eq!(err.log_detail(), "internal error: boom");
    /// ```
    pub fn log_detail(&self) -> String {
        match self {
            Self::Upstream { status, info } => format!(
                "upstream error {status} [{}]: {}",
                info.reason, info.original_message
            ),
            Self::Network(info) => format!(
                "network error [{}]: {} ({})",
                info.category, info.user_message, info.technical_details
            ),
            other => other.to_string(),
        }
    }

    /// Emits this error as a `tracing` event so it reaches every installed
    /// log sink (terminal, GUI log view). Errors that map to a 5xx status
    /// are logged at `ERROR`, everything else at `WARN`. `context`
    /// describes the operation that failed (e.g. `"POST /v1/messages"`).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::GatewayError;
    ///
    /// GatewayError::UnknownModel("gpt-5".into()).report("POST /v1/chat/completions");
    /// ```
    pub fn report(&self, context: &str) {
        let status = self.http_status();
        let detail = self.log_detail();
        if status >= 500 {
            tracing::error!(status, error = %detail, "{context} failed");
        } else {
            tracing::warn!(status, error = %detail, "{context} failed");
        }
    }
}

impl From<rusqlite::Error> for GatewayError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

/// Convenience alias for `Result<T, GatewayError>`, used as the standard
/// result type throughout the crate.
pub type Result<T> = std::result::Result<T, GatewayError>;

/// The `reason` field Kiro attaches to an upstream error payload, classified
/// into known categories so callers can branch on it without string
/// matching everywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KiroErrorReason {
    /// The conversation/content exceeds the model's context window.
    ContentLengthExceedsThreshold,
    /// The account has exhausted its monthly request quota.
    MonthlyRequestCount,
    /// The requested model id is invalid or unavailable for this subscription.
    InvalidModelId,
    /// Kiro did not report a reason, or reported the literal "UNKNOWN".
    Unknown,
    /// A reason string Kiro sent that doesn't match a known category.
    Other(String),
}

impl KiroErrorReason {
    /// Returns the canonical string form of this reason, matching Kiro's own
    /// `reason` field values where applicable.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::error::KiroErrorReason;
    ///
    /// assert_eq!(
    ///     KiroErrorReason::MonthlyRequestCount.as_str(),
    ///     "MONTHLY_REQUEST_COUNT"
    /// );
    /// ```
    pub fn as_str(&self) -> &str {
        match self {
            Self::ContentLengthExceedsThreshold => "CONTENT_LENGTH_EXCEEDS_THRESHOLD",
            Self::MonthlyRequestCount => "MONTHLY_REQUEST_COUNT",
            Self::InvalidModelId => "INVALID_MODEL_ID",
            Self::Unknown => "UNKNOWN",
            Self::Other(s) => s,
        }
    }

    // Maps a raw `reason` string from Kiro's JSON payload to a known
    // variant, preserving unrecognized values via `Other` instead of
    // discarding information.
    fn from_raw(s: &str) -> Self {
        match s {
            "CONTENT_LENGTH_EXCEEDS_THRESHOLD" => Self::ContentLengthExceedsThreshold,
            "MONTHLY_REQUEST_COUNT" => Self::MonthlyRequestCount,
            "INVALID_MODEL_ID" => Self::InvalidModelId,
            "UNKNOWN" => Self::Unknown,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for KiroErrorReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classified, user-facing information about a Kiro upstream error, produced
/// by [`enhance_kiro_error`] and carried inside [`GatewayError::Upstream`].
#[derive(Debug, Clone, Serialize)]
pub struct KiroErrorInfo {
    /// Canonical reason string (see [`KiroErrorReason::as_str`]).
    pub reason: String,
    /// Human-friendly message safe to show to end users.
    pub user_message: String,
    /// The raw `message` field as reported by Kiro, preserved for logs/debugging.
    pub original_message: String,
}

impl fmt::Display for KiroErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]", self.user_message, self.reason)
    }
}

const IMPROPERLY_FORMED_HELP: &str =
    "Kiro API rejected the request. If the problem persists, check the debug logs for details.";

/// Classifies a raw Kiro error JSON payload into a [`KiroErrorInfo`] with a
/// stable `reason` and a human-friendly `user_message`.
///
/// Known reasons (context length, monthly quota, invalid model) get a
/// tailored, actionable message. A generic "Improperly formed request."
/// message with no (or a `"null"`) reason is treated as an unclassified
/// Kiro-side rejection and gets a generic hint to check the debug logs.
/// Any other reason is appended to the original message for visibility
/// without hiding Kiro's own wording.
///
/// # Examples
///
/// ```
/// use lanius_core::error::enhance_kiro_error;
/// use serde_json::json;
///
/// let info = enhance_kiro_error(&json!({
///     "message": "Input is too long.",
///     "reason": "CONTENT_LENGTH_EXCEEDS_THRESHOLD"
/// }));
/// assert_eq!(info.reason, "CONTENT_LENGTH_EXCEEDS_THRESHOLD");
/// ```
pub fn enhance_kiro_error(error_json: &Value) -> KiroErrorInfo {
    let original_message = error_json
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Unknown error")
        .to_string();

    let reason_key_present = error_json.get("reason").is_some();
    let raw_reason = error_json
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN");
    let reason = KiroErrorReason::from_raw(raw_reason);

    let user_message = match &reason {
        KiroErrorReason::ContentLengthExceedsThreshold => {
            "Model context limit reached. Conversation size exceeds model capacity.".to_string()
        }
        KiroErrorReason::MonthlyRequestCount => {
            "Monthly request limit exceeded. Account has reached its monthly quota.".to_string()
        }
        KiroErrorReason::InvalidModelId => {
            "Invalid model ID or insufficient subscription level to use it.".to_string()
        }
        // Kiro's generic "Improperly formed request." message with no usable
        // reason gives the caller nothing actionable, so we point them at
        // the debug logs instead of surfacing an empty error.
        _ if original_message == "Improperly formed request." && is_reasonless(&reason) => {
            IMPROPERLY_FORMED_HELP.to_string()
        }
        KiroErrorReason::Unknown => original_message.clone(),
        other => {
            // Unrecognized-but-present reasons are appended rather than
            // replacing the original message, so operators can still see
            // exactly what Kiro said even for reasons we don't special-case.
            if reason_key_present {
                format!("{original_message} (reason: {other})")
            } else {
                original_message.clone()
            }
        }
    };

    KiroErrorInfo {
        reason: reason.as_str().to_string(),
        user_message,
        original_message,
    }
}

// A reason counts as "absent" for the improperly-formed-request special case
// when Kiro omitted it entirely (`Unknown`) or explicitly sent the string
// "null" (as opposed to a real, if unrecognized, reason code).
fn is_reasonless(reason: &KiroErrorReason) -> bool {
    match reason {
        KiroErrorReason::Unknown => true,
        KiroErrorReason::Other(s) => s == "null",
        _ => false,
    }
}

/// Category of transport-level failure encountered while talking to Kiro,
/// as classified by [`classify_network_error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// DNS lookup for the upstream host failed.
    DnsResolution,
    /// The upstream host actively refused the connection.
    ConnectionRefused,
    /// An established connection was reset by the peer.
    ConnectionReset,
    /// The upstream network was unreachable.
    NetworkUnreachable,
    /// Timed out while establishing the connection.
    TimeoutConnect,
    /// Timed out while waiting to read a response.
    TimeoutRead,
    /// TLS/SSL handshake or certificate validation failed.
    SslError,
    /// The configured HTTP/SOCKS proxy failed.
    ProxyError,
    /// Too many HTTP redirects were followed.
    TooManyRedirects,
    /// The error did not match any of the categories above.
    Unknown,
}

impl ErrorCategory {
    /// Returns the `snake_case` string form of this category, matching its
    /// serialized representation.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::error::ErrorCategory;
    ///
    /// assert_eq!(ErrorCategory::DnsResolution.as_str(), "dns_resolution");
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DnsResolution => "dns_resolution",
            Self::ConnectionRefused => "connection_refused",
            Self::ConnectionReset => "connection_reset",
            Self::NetworkUnreachable => "network_unreachable",
            Self::TimeoutConnect => "timeout_connect",
            Self::TimeoutRead => "timeout_read",
            Self::SslError => "ssl_error",
            Self::ProxyError => "proxy_error",
            Self::TooManyRedirects => "too_many_redirects",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classified, user-facing information about a transport-level network
/// failure, produced by [`classify_network_error`] and carried inside
/// [`GatewayError::Network`].
#[derive(Debug, Clone, Serialize)]
pub struct NetworkErrorInfo {
    /// Broad category of failure (DNS, TLS, timeout, etc.).
    pub category: ErrorCategory,
    /// Human-friendly message safe to show to end users.
    pub user_message: String,
    /// Ordered list of suggestions for the user/operator to try.
    pub troubleshooting_steps: Vec<String>,
    /// Full `Debug` rendering of the original `reqwest::Error`, for logs only.
    pub technical_details: String,
    /// Whether retrying the same request is likely to succeed.
    pub is_retryable: bool,
    /// HTTP status code recommended when surfacing this error to clients.
    pub suggested_http_code: u16,
}

impl fmt::Display for NetworkErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.category, self.user_message)
    }
}

/// Formats a compact `"category: message"` string, useful for single-line
/// logging of a [`NetworkErrorInfo`] without its troubleshooting steps.
///
/// # Examples
///
/// ```
/// use lanius_core::error::{short_error_message, ErrorCategory, NetworkErrorInfo};
///
/// let info = NetworkErrorInfo {
///     category: ErrorCategory::DnsResolution,
///     user_message: "DNS resolution failed.".into(),
///     troubleshooting_steps: vec![],
///     technical_details: String::new(),
///     is_retryable: true,
///     suggested_http_code: 502,
/// };
/// assert_eq!(short_error_message(&info), "dns_resolution: DNS resolution failed.");
/// ```
pub fn short_error_message(info: &NetworkErrorInfo) -> String {
    format!("{}: {}", info.category, info.user_message)
}

/// Classifies a raw `reqwest::Error` from an upstream request into a
/// [`NetworkErrorInfo`] with a specific category, a human-friendly message,
/// troubleshooting steps, and retryability.
///
/// Classification order matters: redirects and explicit timeouts are
/// checked first via `reqwest`'s own flags, then the full error source chain
/// (lower-cased) is scanned for proxy/TLS/DNS/connection-refused/reset/
/// unreachable substrings, since the underlying `hyper`/`std::io` errors
/// don't always expose a structured kind. Anything unmatched falls back to
/// [`ErrorCategory::Unknown`] but is still treated as retryable, since most
/// unclassified transport errors are transient.
///
/// # Examples
///
/// ```no_run
/// use lanius_core::error::classify_network_error;
///
/// # async fn example() {
/// // A request to an unreachable host produces a classified, retryable error.
/// let result = reqwest::get("http://127.0.0.1:1/does-not-exist").await;
/// if let Err(error) = result {
///     let info = classify_network_error(&error);
///     println!("{}: {}", info.category, info.user_message);
/// }
/// # }
/// ```
pub fn classify_network_error(error: &reqwest::Error) -> NetworkErrorInfo {
    let technical_details = format!("{error:?}");
    let chain = error_source_chain(error).to_ascii_lowercase();

    // reqwest flags redirect-loop failures explicitly; check this before
    // string-matching the source chain.
    if error.is_redirect() {
        return NetworkErrorInfo {
            category: ErrorCategory::TooManyRedirects,
            user_message: "Too many redirects - the server is redirecting in a loop.".into(),
            troubleshooting_steps: vec![
                "This is likely a server-side configuration issue".into(),
                "Try accessing the service directly without the gateway".into(),
                "Contact the service provider if the issue persists".into(),
            ],
            technical_details,
            is_retryable: false,
            suggested_http_code: 502,
        };
    }

    if error.is_timeout() {
        // Distinguish "never connected" from "connected but the response
        // stalled" since the troubleshooting advice differs.
        let (category, user_message) = if error.is_connect() {
            (
                ErrorCategory::TimeoutConnect,
                "Connection timed out - could not establish a connection to the Kiro API.",
            )
        } else {
            (
                ErrorCategory::TimeoutRead,
                "Read timed out - the Kiro API accepted the connection but stopped responding.",
            )
        };
        return NetworkErrorInfo {
            category,
            user_message: user_message.into(),
            troubleshooting_steps: vec![
                "Check your internet connection stability".into(),
                "If you are behind a VPN or proxy, verify it is reachable".into(),
                "Increase the first-token / streaming read timeout in settings".into(),
            ],
            technical_details,
            is_retryable: true,
            suggested_http_code: 504,
        };
    }

    if chain.contains("proxy") || chain.contains("socks") {
        return NetworkErrorInfo {
            category: ErrorCategory::ProxyError,
            user_message: "Proxy connection failed - cannot connect through the configured proxy."
                .into(),
            troubleshooting_steps: vec![
                "Check the VPN/Proxy URL in settings (HTTP and SOCKS5 are supported)".into(),
                "Verify the proxy server is running and reachable".into(),
                "Try disabling the proxy temporarily".into(),
                "Check proxy authentication credentials if required".into(),
            ],
            technical_details,
            is_retryable: true,
            suggested_http_code: 502,
        };
    }

    // rustls reports a peer that drops the TCP connection mid-response as
    // "peer closed connection without sending TLS close_notify". That is a
    // connection drop, not a handshake/certificate problem, so it must be
    // matched before the TLS substring check below.
    if is_unclean_tls_eof(&chain) {
        return NetworkErrorInfo {
            category: ErrorCategory::ConnectionReset,
            user_message: "Connection closed unexpectedly - the upstream or an intermediate proxy dropped the connection."
                .into(),
            troubleshooting_steps: vec![
                "Retry the request".into(),
                "Check for a proxy, VPN or firewall terminating long-lived connections".into(),
            ],
            technical_details,
            is_retryable: true,
            suggested_http_code: 502,
        };
    }

    if chain.contains("certificate")
        || chain.contains("tls")
        || chain.contains("ssl")
        || chain.contains("handshake")
    {
        return NetworkErrorInfo {
            category: ErrorCategory::SslError,
            user_message: "TLS handshake failed - the secure connection could not be established."
                .into(),
            troubleshooting_steps: vec![
                "Check the system clock; a wrong date invalidates certificates".into(),
                "If a corporate proxy inspects TLS, its root CA must be trusted".into(),
                "Verify no firewall is intercepting HTTPS traffic".into(),
            ],
            technical_details,
            is_retryable: false,
            suggested_http_code: 502,
        };
    }

    if error.is_connect() {
        // The connect-phase source chain is scanned for OS/DNS-resolver
        // substrings since `reqwest`/`hyper` don't expose a structured
        // error kind for these cases; order matters as DNS failures often
        // also mention "lookup" alongside "connect".
        let (category, user_message, retryable) = if chain.contains("dns")
            || chain.contains("name or service not known")
            || chain.contains("failed to lookup address")
            || chain.contains("nodename nor servname")
        {
            (
                ErrorCategory::DnsResolution,
                "DNS resolution failed - the Kiro API hostname could not be resolved.",
                true,
            )
        } else if chain.contains("connection refused") {
            (
                ErrorCategory::ConnectionRefused,
                "Connection refused - nothing accepted the connection on the target host.",
                true,
            )
        } else if chain.contains("connection reset") {
            (
                ErrorCategory::ConnectionReset,
                "Connection reset - the connection was closed unexpectedly.",
                true,
            )
        } else if chain.contains("network is unreachable")
            || chain.contains("no route to host")
            || chain.contains("unreachable")
        {
            (
                ErrorCategory::NetworkUnreachable,
                "Network unreachable - no route to the Kiro API.",
                true,
            )
        } else {
            (
                ErrorCategory::Unknown,
                "Could not connect to the Kiro API.",
                true,
            )
        };

        return NetworkErrorInfo {
            category,
            user_message: user_message.into(),
            troubleshooting_steps: vec![
                "Check your internet connection".into(),
                "Verify the Kiro API is reachable from this network".into(),
                "If you are in a restricted network, configure the VPN/Proxy URL".into(),
            ],
            technical_details,
            is_retryable: retryable,
            suggested_http_code: 502,
        };
    }

    if chain.contains("connection reset") || chain.contains("broken pipe") {
        return NetworkErrorInfo {
            category: ErrorCategory::ConnectionReset,
            user_message: "Connection reset - the connection was closed unexpectedly.".into(),
            troubleshooting_steps: vec![
                "Retry the request".into(),
                "Check for a proxy or firewall terminating long-lived connections".into(),
            ],
            technical_details,
            is_retryable: true,
            suggested_http_code: 502,
        };
    }

    NetworkErrorInfo {
        category: ErrorCategory::Unknown,
        user_message: "Network request failed due to an unexpected error.".into(),
        troubleshooting_steps: vec![
            "Retry the request".into(),
            "Check the application logs for the technical details".into(),
        ],
        technical_details,
        is_retryable: true,
        suggested_http_code: 502,
    }
}

// Expects an already lower-cased source chain.
fn is_unclean_tls_eof(chain: &str) -> bool {
    chain.contains("close_notify") || chain.contains("unexpected eof")
}

// Flattens `error.source()` into a single lower-case-able string so the
// classification checks above can substring-match across the whole chain
// (reqwest -> hyper -> std::io, etc.) instead of only the top-level message.
fn error_source_chain(error: &reqwest::Error) -> String {
    use std::error::Error as _;

    let mut parts = vec![error.to_string()];
    let mut cursor: Option<&(dyn std::error::Error + 'static)> = error.source();
    for _ in 0..16 {
        match cursor {
            Some(e) => {
                parts.push(e.to_string());
                cursor = e.source();
            }
            None => break,
        }
    }
    parts.join(" | ")
}

impl From<reqwest::Error> for GatewayError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(Box::new(classify_network_error(&e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enhances_context_length_exceeded() {
        let info = enhance_kiro_error(&json!({
            "message": "Input is too long.",
            "reason": "CONTENT_LENGTH_EXCEEDS_THRESHOLD"
        }));
        assert_eq!(info.reason, "CONTENT_LENGTH_EXCEEDS_THRESHOLD");
        assert_eq!(
            info.user_message,
            "Model context limit reached. Conversation size exceeds model capacity."
        );
        assert_eq!(info.original_message, "Input is too long.");
    }

    #[test]
    fn enhances_monthly_request_count() {
        let info = enhance_kiro_error(&json!({
            "message": "Quota exceeded.",
            "reason": "MONTHLY_REQUEST_COUNT"
        }));
        assert_eq!(
            info.user_message,
            "Monthly request limit exceeded. Account has reached its monthly quota."
        );
    }

    #[test]
    fn enhances_invalid_model_id() {
        let info = enhance_kiro_error(&json!({
            "message": "bad model",
            "reason": "INVALID_MODEL_ID"
        }));
        assert_eq!(
            info.user_message,
            "Invalid model ID or insufficient subscription level to use it."
        );
    }

    #[test]
    fn appends_reason_for_unknown_reason() {
        let info = enhance_kiro_error(&json!({
            "message": "Something went wrong.",
            "reason": "UNKNOWN_REASON"
        }));
        assert_eq!(
            info.user_message,
            "Something went wrong. (reason: UNKNOWN_REASON)"
        );
    }

    #[test]
    fn missing_message_becomes_unknown_error() {
        let info = enhance_kiro_error(&json!({}));
        assert_eq!(info.original_message, "Unknown error");
        assert_eq!(info.reason, "UNKNOWN");
        assert_eq!(info.user_message, "Unknown error");
    }

    #[test]
    fn null_message_and_reason_default_to_unknown() {
        let info = enhance_kiro_error(&json!({"message": null, "reason": null}));
        assert_eq!(info.original_message, "Unknown error");
        assert_eq!(info.reason, "UNKNOWN");
        assert_eq!(info.user_message, "Unknown error");
    }

    #[test]
    fn empty_message_is_preserved_not_replaced() {
        let info = enhance_kiro_error(&json!({"message": ""}));
        assert_eq!(info.original_message, "");
        assert_eq!(info.user_message, "");
    }

    #[test]
    fn generic_improperly_formed_request_gets_debug_log_hint() {
        let info = enhance_kiro_error(&json!({"message": "Improperly formed request."}));
        assert!(info.user_message.contains("debug logs"));
    }

    #[test]
    fn reasonless_guard_covers_null_string() {
        let info = enhance_kiro_error(&json!({
            "message": "Improperly formed request.",
            "reason": "null"
        }));
        assert!(info.user_message.contains("debug logs"));
    }

    #[test]
    fn unclean_tls_eof_is_not_a_handshake_failure() {
        let chain = "error decoding response body | request or response body error | \
                     peer closed connection without sending tls close_notify: \
                     https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof";
        assert!(is_unclean_tls_eof(chain));
        assert!(!is_unclean_tls_eof(
            "invalid peer certificate: unknownissuer"
        ));
    }

    #[test]
    fn log_detail_keeps_diagnostics_hidden_from_users() {
        let err = GatewayError::Upstream {
            status: 400,
            info: Box::new(enhance_kiro_error(&json!({
                "message": "Improperly formed request.",
            }))),
        };
        assert!(err.user_message().contains("debug logs"));
        let detail = err.log_detail();
        assert!(detail.contains("400"), "{detail}");
        assert!(detail.contains("Improperly formed request."), "{detail}");
    }

    #[test]
    fn report_logs_5xx_as_error_and_others_as_warn() {
        let (logs, _guard) = crate::test_log::CapturedLogs::install();
        GatewayError::Internal("boom".into()).report("POST /x");
        GatewayError::UnknownModel("gpt-5".into()).report("POST /y");
        assert!(
            logs.has(
                "ERROR",
                &["POST /x failed", "status=500", "internal error: boom"]
            ),
            "{:?}",
            logs.lines()
        );
        assert!(
            logs.has(
                "WARN",
                &["POST /y failed", "status=400", "unknown model: gpt-5"]
            ),
            "{:?}",
            logs.lines()
        );
    }

    #[test]
    fn http_status_mapping() {
        assert_eq!(GatewayError::Unauthorized("x".into()).http_status(), 401);
        assert_eq!(
            GatewayError::FirstTokenTimeout(std::time::Duration::from_secs(1)).http_status(),
            504
        );
        assert_eq!(
            GatewayError::Upstream {
                status: 429,
                info: Box::new(KiroErrorInfo {
                    reason: "UNKNOWN".into(),
                    user_message: "rate limited".into(),
                    original_message: "rate limited".into(),
                })
            }
            .http_status(),
            429
        );
    }
}
