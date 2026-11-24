//! Fingerprinting, user-agent, id generation, and spaced-JSON helpers.
//!
//! Kiro's backend expects requests shaped exactly like an official client's.
//! Lanius identifies as the Kiro CLI 3.5.0 default (V3) agent
//! ([`kiro_headers`], [`kiro_user_agent`]), keeps a stable per-machine fingerprint ([`machine_fingerprint`]) for
//! diagnostics, and serializes JSON payloads serialized with `", "`/`": "` separators rather than
//! `serde_json`'s compact default — see [`format_json_spaced`]/
//! [`format_json_spaced_sorted`], used by `upstream::parser` and
//! [`crate::tokenizer`] to match Kiro's expected wire format and token
//! accounting byte-for-byte. This module also provides the various opaque
//! id generators used across the crate ([`generate_completion_id`],
//! [`generate_message_id`], [`generate_tool_call_id`],
//! [`generate_conversation_id`]).

use once_cell::sync::Lazy;
use serde_json::Value;
use sha2::{Digest, Sha256};

const FINGERPRINT_SALT: &str = "lanius";

const FINGERPRINT_FALLBACK: &[u8] = b"default-lanius";

// Computed once per process and cached, since the fingerprint depends only
// on hostname/username, which do not change while the process is running.
static FINGERPRINT: Lazy<String> = Lazy::new(compute_machine_fingerprint);

/// Returns the stable, per-process machine fingerprint (shown in diagnostics;
/// not sent upstream, since the Kiro CLI does not). Computed lazily on first access
/// and cached for the lifetime of the process.
///
/// # Examples
///
/// ```
/// let fp = lanius_core::utils::machine_fingerprint();
/// assert_eq!(fp.len(), 64, "SHA-256 hex digest is 64 chars");
/// assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
/// ```
pub fn machine_fingerprint() -> &'static str {
    &FINGERPRINT
}

// Derives the fingerprint as `sha256(hostname-username-salt)`. Falls back to
// hashing a fixed placeholder if hostname/username cannot be determined
// (e.g. in a sandboxed environment), so the gateway still starts rather than
// failing outright.
fn compute_machine_fingerprint() -> String {
    let hostname = whoami::hostname().ok();
    let username = whoami::username().ok();

    match (hostname, username) {
        (Some(h), Some(u)) => {
            let pre_image = format!("{h}-{u}-{FINGERPRINT_SALT}");
            hex_sha256(pre_image.as_bytes())
        }
        _ => {
            tracing::warn!("failed to determine hostname/username for machine fingerprint");
            hex_sha256(FINGERPRINT_FALLBACK)
        }
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Hashes a client-supplied session identifier or a stable conversation seed
/// before it is retained for API-key stickiness.
pub fn hash_affinity_key(value: &str) -> String {
    hex_sha256(value.as_bytes())[..32].to_string()
}

// Client identity mirrored from the official Kiro CLI 3.5.0, whose default V3
// agent harness (KAS, `@kiro/agent` 0.66.119) talks to Kiro through the AWS
// SDK for JavaScript on Node.js 22.23.4 (captured from live requests). Bump
// these together when Kiro raises its minimum supported client version.
const KIRO_CLI_VERSION: &str = "3.5.0";
const KAS_VERSION: &str = "0.66.119";
const NODE_VERSION: &str = "22.23.4";
const KIRO_CLI_APP: &str = "app/AmazonQ-For-CLI";
/// ACP client name of the interactive Kiro CLI (`kiro-cli-non-interactive`
/// in headless runs).
const ACP_CLIENT: &str = "acp-client/kiro-cli";

/// Smithy service identifier for the runtime API (`KiroRuntimeService`:
/// chat and `InvokeMCP`).
pub const API_RUNTIME: &str = "kiroruntime#1.0.0";
/// Smithy service identifier for the control-plane API
/// (`KiroControlPlaneBearerService`: models, profile, usage).
pub const API_CONTROL_PLANE_BEARER: &str = "kirocontrolplanebearer#1.0.0";

/// `x-amz-target` prefix of the runtime API.
pub const RUNTIME_SERVICE: &str = "KiroRuntimeService";
/// `x-amz-target` prefix of the control-plane API.
pub const CONTROL_PLANE_SERVICE: &str = "KiroControlPlaneBearerService";

/// `origin` value the Kiro CLI sends in chat payloads and runtime API bodies.
pub const KIRO_ORIGIN: &str = "KIRO_CLI";

/// `x-amzn-kiro-client-attribution` value of the Kiro CLI.
pub const KIRO_CLIENT_ATTRIBUTION: &str = "kiro-cli";

/// Agent mode of the Kiro CLI's default agent: the chat request's top-level
/// `agentMode` and its `x-amzn-kiro-mode-identifier` header.
pub const KIRO_AGENT_MODE: &str = "default-v2";

/// `TokenType` header value that tells Kiro the bearer token is an API key.
/// Without it Kiro treats the key as an OAuth token and answers `profileArn is required`.
pub const TOKEN_TYPE_API_KEY: &str = "API_KEY";

/// `TokenType` header value the Kiro CLI sends for IAM Identity Center
/// (AWS SSO OIDC) tokens. Social-login tokens are sent without a `TokenType`.
pub const TOKEN_TYPE_SSO_OIDC: &str = "SSO_OIDC";

/// `User-Agent` value the Kiro CLI sends on social-login token refresh.
pub const KIRO_CLI_REFRESH_USER_AGENT: &str = "Kiro-CLI";

/// Smithy service identifier for the AWS SSO OIDC token API, whose refresh
/// the Kiro CLI still performs from its Rust binary.
pub const API_SSO_OIDC: &str = "ssooidc/1.100.0";
const AWS_SDK_RUST_VERSION: &str = "1.6.0";
const RUST_VERSION: &str = "1.94.1";

/// Node.js `os.platform()` value (`darwin`, `linux`, `win32`) the Kiro CLI's
/// agent harness reports in its `os/` user-agent components.
///
/// # Examples
///
/// ```
/// let os = lanius_core::utils::kiro_os();
/// assert!(["darwin", "linux", "win32"].contains(&os) || !os.is_empty());
/// ```
pub fn kiro_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// The kernel release (Node.js `os.release()`) that follows `os/<platform>#`
/// in the user agent. Read once; `unknown` where it cannot be determined.
fn os_release() -> &'static str {
    static RELEASE: Lazy<String> = Lazy::new(|| {
        let release = match std::env::consts::OS {
            "linux" => std::fs::read_to_string("/proc/sys/kernel/osrelease").ok(),
            "macos" => command_output("uname", &["-r"]),
            "windows" => windows_release(),
            _ => None,
        };
        release
            .map(|release| release.trim().to_owned())
            .filter(|release| !release.is_empty() && !release.contains(char::is_whitespace))
            .unwrap_or_else(|| "unknown".to_owned())
    });
    &RELEASE
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `10.0.<build>` from `cmd /c ver`, the form Node.js reports on Windows.
fn windows_release() -> Option<String> {
    let text = command_output("cmd", &["/c", "ver"])?;
    let start = text.find("10.")?;
    let version: String = text[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect();
    let mut parts = version.split('.');
    Some(format!(
        "{}.{}.{}",
        parts.next()?,
        parts.next()?,
        parts.next()?
    ))
}

/// The client-identity tail shared by both user agents.
fn kiro_client_tag() -> String {
    format!(
        "KiroCLI/{KIRO_CLI_VERSION} KAS/{KAS_VERSION} os/{os} md/appVersion-{KIRO_CLI_VERSION} \
         {KIRO_CLI_APP} {ACP_CLIENT}",
        os = kiro_os()
    )
}

/// Kiro CLI `User-Agent` for the smithy client identified by `api`.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::{API_RUNTIME, kiro_user_agent};
///
/// let ua = kiro_user_agent(API_RUNTIME);
/// assert!(ua.starts_with("aws-sdk-js/1.0.0 ua/2.1 os/"));
/// assert!(ua.contains(" api/kiroruntime#1.0.0 m/N KiroCLI/"));
/// assert!(ua.ends_with("app/AmazonQ-For-CLI acp-client/kiro-cli"));
/// ```
pub fn kiro_user_agent(api: &str) -> String {
    // The control-plane client also records the `E` (endpoint override)
    // feature id, since KAS sets its endpoint explicitly.
    let features = if api == API_CONTROL_PLANE_BEARER {
        "N,E"
    } else {
        "N"
    };
    format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{os}#{release} lang/js md/nodejs#{NODE_VERSION} api/{api} \
         m/{features} {tag}",
        os = kiro_os(),
        release = os_release(),
        tag = kiro_client_tag()
    )
}

/// Kiro CLI `x-amz-user-agent`, the same for every smithy client.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::kiro_amz_user_agent;
///
/// assert!(kiro_amz_user_agent().starts_with("aws-sdk-js/1.0.0 KiroCLI/"));
/// ```
pub fn kiro_amz_user_agent() -> String {
    format!("aws-sdk-js/1.0.0 {}", kiro_client_tag())
}

/// `User-Agent` / `x-amz-user-agent` pair for a stock AWS SDK client (e.g. SSO OIDC),
/// which uses the short `User-Agent` form without the Kiro app version metadata.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::{API_SSO_OIDC, aws_sdk_user_agents};
///
/// let (ua, amz) = aws_sdk_user_agents(API_SSO_OIDC);
/// assert!(!ua.contains("api/"));
/// assert!(amz.contains("api/ssooidc/"));
/// ```
pub fn aws_sdk_user_agents(api: &str) -> (String, String) {
    let os = std::env::consts::OS;
    (
        format!("aws-sdk-rust/{AWS_SDK_RUST_VERSION} os/{os} lang/rust/{RUST_VERSION}"),
        format!(
            "aws-sdk-rust/{AWS_SDK_RUST_VERSION} ua/2.1 api/{api} os/{os} lang/rust/{RUST_VERSION} \
             {KIRO_CLI_APP}"
        ),
    )
}

/// Builds the Kiro CLI header set for a request to the smithy service `api`:
/// bearer auth, AWS JSON content type, CLI user-agents, and a fresh
/// `amz-sdk-invocation-id` per call. `x-amz-target`, `TokenType` and the
/// operation-specific `x-amzn-kiro-*` headers are added by callers.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::{API_RUNTIME, kiro_headers_for};
///
/// let headers = kiro_headers_for("tok", API_RUNTIME);
/// assert!(headers.iter().any(|(k, v)| *k == "User-Agent" && v.contains("kiroruntime")));
/// ```
pub fn kiro_headers_for(token: &str, api: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("Content-Type", "application/x-amz-json-1.0".to_string()),
        ("User-Agent", kiro_user_agent(api)),
        ("x-amz-user-agent", kiro_amz_user_agent()),
        // The Kiro CLI sends this header when the user has not opted in to
        // content collection; Lanius keeps that default.
        ("x-amzn-codewhisperer-optout", "true".to_string()),
        ("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string()),
        ("amz-sdk-request", "attempt=1; max=3".to_string()),
    ]
}

/// Builds the full set of headers for a Kiro `GenerateAssistantResponse`
/// request, matching what the Kiro CLI's default agent sends.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::kiro_headers;
///
/// let headers = kiro_headers("my-token");
/// assert!(headers.iter().any(|(k, v)| *k == "Authorization" && v == "Bearer my-token"));
/// assert!(headers.iter().any(|(k, v)| *k == "x-amzn-kiro-mode-identifier" && v == "default-v2"));
/// ```
pub fn kiro_headers(token: &str) -> Vec<(&'static str, String)> {
    let mut headers = kiro_headers_for(token, API_RUNTIME);
    headers.push((
        "x-amz-target",
        format!("{RUNTIME_SERVICE}.GenerateAssistantResponse"),
    ));
    headers.push((
        "x-amzn-kiro-client-attribution",
        KIRO_CLIENT_ATTRIBUTION.to_string(),
    ));
    headers.push(("x-amzn-kiro-mode-identifier", KIRO_AGENT_MODE.to_string()));
    headers.push(("x-amzn-kiro-fallback", "0;max=0".to_string()));
    headers.push(("x-amzn-kiro-attempt", "1;max=3".to_string()));
    headers
}

/// Generates a fresh `agentContinuationId` (a UUID). The Kiro CLI mints one
/// per user turn; Lanius, which keeps no state, mints one per request.
///
/// # Examples
///
/// ```
/// let id = lanius_core::utils::generate_continuation_id();
/// assert_eq!(id.len(), 36);
/// ```
pub fn generate_continuation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Generates an OpenAI-style chat-completion id: `chatcmpl-<32 hex chars>`.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::generate_completion_id;
///
/// let id = generate_completion_id();
/// assert!(id.starts_with("chatcmpl-"));
/// ```
pub fn generate_completion_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
}

/// Generates an Anthropic-style message id: `msg_<32 hex chars>`.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::generate_message_id;
///
/// let id = generate_message_id();
/// assert!(id.starts_with("msg_"));
/// ```
pub fn generate_message_id() -> String {
    format!("msg_{}", uuid::Uuid::new_v4().simple())
}

/// Generates an OpenAI-style tool-call id: `call_<8 hex chars>`.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::generate_tool_call_id;
///
/// let id = generate_tool_call_id();
/// assert!(id.starts_with("call_"));
/// assert_eq!(id.len(), "call_".len() + 8);
/// ```
pub fn generate_tool_call_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("call_{}", &hex[..8])
}

/// A minimal, borrowed view of a chat message (`role` + `content`), used as
/// input to [`generate_conversation_id`] without requiring callers to
/// convert their own message types first.
pub struct HashableMessage<'a> {
    /// The message's role (e.g. `"user"`, `"assistant"`).
    pub role: &'a str,
    /// The message's content, in whatever JSON shape the caller's protocol
    /// uses.
    pub content: &'a Value,
}

/// Derives a short, stable conversation id from a list of messages, intended
/// to stay constant across turns of the *same* conversation while changing
/// when the conversation's substance changes.
///
/// Only the first three messages plus the very last message are hashed
/// (for conversations longer than three messages) — this keeps the id
/// stable as new messages are appended in the middle/end of a long
/// conversation (matching how many chat UIs incrementally grow history)
/// while still changing if the opening context or the most recent message
/// differs. An empty message list has no stable identity to derive, so a
/// fresh random UUID is returned instead (deliberately *not* stable across
/// calls). Content is stringified and truncated to 100 characters before
/// hashing, then reduced to a 16-character hex digest via SHA-256.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::{generate_conversation_id, HashableMessage};
/// use serde_json::json;
///
/// let content = json!("Hello");
/// let messages = [HashableMessage { role: "user", content: &content }];
/// let id = generate_conversation_id(&messages);
/// assert_eq!(id.len(), 16);
/// // Calling it again with the same messages yields the same id.
/// assert_eq!(id, generate_conversation_id(&messages));
/// ```
pub fn generate_conversation_id(messages: &[HashableMessage<'_>]) -> String {
    if messages.is_empty() {
        return uuid::Uuid::new_v4().to_string();
    }

    let key_messages: Vec<&HashableMessage<'_>> = if messages.len() <= 3 {
        messages.iter().collect()
    } else {
        messages[..3]
            .iter()
            .chain(std::iter::once(&messages[messages.len() - 1]))
            .collect()
    };

    let mut parts = Vec::with_capacity(key_messages.len());
    for msg in key_messages {
        let content_str = truncate_chars(&stringify_content(msg.content), 100);
        parts.push(format!(
            "{{{}: {}, {}: {}}}",
            json_quote("content"),
            json_quote(&content_str),
            json_quote("role"),
            json_quote(msg.role),
        ));
    }
    let pre_image = format!("[{}]", parts.join(", "));

    hex_sha256(pre_image.as_bytes())[..16].to_string()
}

// Reduces a message's `content` value to a plain string for hashing:
// strings pass through unchanged, arrays (content-block form) are rendered
// via sorted-key spaced JSON so their hash is stable regardless of
// incidental key ordering, and null becomes an empty string.
fn stringify_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(_) => format_json_spaced_sorted(content),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Serializes `value` using `", "` / `": "` separators and keys in their
/// original insertion order (rather than `serde_json`'s compact default),
/// matching the exact wire format Kiro expects.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::format_json_spaced;
/// use serde_json::json;
///
/// let v = json!({"role": "user", "content": "hi"});
/// assert_eq!(format_json_spaced(&v), r#"{"role": "user", "content": "hi"}"#);
/// ```
pub fn format_json_spaced(value: &Value) -> String {
    let mut out = String::new();
    write_spaced_json(value, false, &mut out);
    out
}

/// Like [`format_json_spaced`], but additionally sorts object keys,
/// used where a stable, order-independent serialization is required
/// (e.g. hashing).
///
/// # Examples
///
/// ```
/// use lanius_core::utils::format_json_spaced_sorted;
/// use serde_json::json;
///
/// let v = json!({"z": 1, "a": 2});
/// assert_eq!(format_json_spaced_sorted(&v), r#"{"a": 2, "z": 1}"#);
/// ```
pub fn format_json_spaced_sorted(value: &Value) -> String {
    let mut out = String::new();
    write_spaced_json(value, true, &mut out);
    out
}

fn write_spaced_json(value: &Value, sort_keys: bool, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&json_quote(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_spaced_json(item, sort_keys, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            if sort_keys {
                keys.sort();
            }
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&json_quote(k));
                out.push_str(": ");
                write_spaced_json(&map[*k], sort_keys, out);
            }
            out.push('}');
        }
    }
}

fn json_quote(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

// Truncates to the first `n` Unicode scalar values, safe for multi-byte
// characters (unlike naive byte slicing).
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprint_is_stable_and_hex_sha256() {
        let a = machine_fingerprint();
        let b = machine_fingerprint();
        assert_eq!(a, b, "fingerprint must not change within a process");
        assert_eq!(a.len(), 64, "SHA-256 hex digest is 64 chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_matches_known_sha256_preimage() {
        let expected = hex_sha256(b"myhost-myuser-lanius");
        let actual =
            hex_sha256(format!("{}-{}-{}", "myhost", "myuser", FINGERPRINT_SALT).as_bytes());
        assert_eq!(expected, actual);
        assert_eq!(FINGERPRINT_SALT, "lanius");
    }

    #[test]
    fn user_agents_match_kiro_cli_byte_exact() {
        let os = kiro_os();
        let release = os_release();
        let tag = format!(
            "KiroCLI/3.5.0 KAS/0.66.119 os/{os} md/appVersion-3.5.0 app/AmazonQ-For-CLI \
             acp-client/kiro-cli"
        );
        assert_eq!(
            kiro_user_agent(API_RUNTIME),
            format!(
                "aws-sdk-js/1.0.0 ua/2.1 os/{os}#{release} lang/js md/nodejs#22.23.4 \
                 api/kiroruntime#1.0.0 m/N {tag}"
            )
        );
        assert_eq!(
            kiro_user_agent(API_CONTROL_PLANE_BEARER),
            format!(
                "aws-sdk-js/1.0.0 ua/2.1 os/{os}#{release} lang/js md/nodejs#22.23.4 \
                 api/kirocontrolplanebearer#1.0.0 m/N,E {tag}"
            )
        );
        assert_eq!(kiro_amz_user_agent(), format!("aws-sdk-js/1.0.0 {tag}"));
        let rust_os = std::env::consts::OS;
        assert_eq!(
            aws_sdk_user_agents(API_SSO_OIDC),
            (
                format!("aws-sdk-rust/1.6.0 os/{rust_os} lang/rust/1.94.1"),
                format!(
                    "aws-sdk-rust/1.6.0 ua/2.1 api/ssooidc/1.100.0 os/{rust_os} lang/rust/1.94.1 \
                     app/AmazonQ-For-CLI"
                ),
            )
        );
    }

    #[test]
    fn os_tag_uses_node_platform_names() {
        let expected = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        };
        assert_eq!(kiro_os(), expected);
        assert!(!os_release().is_empty());
        assert!(!os_release().contains(char::is_whitespace));
    }

    #[test]
    fn headers_never_identify_as_kiro_ide_or_the_rust_streaming_client() {
        for (name, value) in kiro_headers("tok") {
            assert!(!value.contains("KiroIDE"), "{name} still claims KiroIDE");
            assert!(
                !value.contains("codewhispererstreaming"),
                "{name} still claims the CLI 2.x streaming client"
            );
        }
    }

    #[test]
    fn header_set_is_complete() {
        let headers = kiro_headers("tok");
        let value = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("missing header {name}"))
        };
        assert_eq!(value("Authorization"), "Bearer tok");
        assert_eq!(value("Content-Type"), "application/x-amz-json-1.0");
        assert_eq!(
            value("x-amz-target"),
            "KiroRuntimeService.GenerateAssistantResponse"
        );
        assert_eq!(value("x-amzn-kiro-client-attribution"), "kiro-cli");
        assert_eq!(value("x-amzn-kiro-mode-identifier"), "default-v2");
        assert_eq!(value("x-amzn-kiro-fallback"), "0;max=0");
        assert_eq!(value("x-amzn-kiro-attempt"), "1;max=3");
        assert_eq!(value("amz-sdk-request"), "attempt=1; max=3");
        assert_eq!(value("x-amzn-codewhisperer-optout"), "true");
        assert!(value("User-Agent").contains("api/kiroruntime#1.0.0"));
        assert!(value("x-amz-user-agent").starts_with("aws-sdk-js/1.0.0 KiroCLI/3.5.0"));
        assert!(headers.iter().all(|(k, _)| *k != "x-kiro-attempt"));
    }

    #[test]
    fn invocation_id_is_fresh_per_call() {
        let a = kiro_headers("t");
        let b = kiro_headers("t");
        let id = |h: &Vec<(&str, String)>| {
            h.iter()
                .find(|(k, _)| *k == "amz-sdk-invocation-id")
                .unwrap()
                .1
                .clone()
        };
        assert_ne!(id(&a), id(&b));
    }

    #[test]
    fn id_formats() {
        let c = generate_completion_id();
        assert!(c.starts_with("chatcmpl-"));
        assert_eq!(c.len(), "chatcmpl-".len() + 32);

        let m = generate_message_id();
        assert!(m.starts_with("msg_"));

        let t = generate_tool_call_id();
        assert!(t.starts_with("call_"));
        assert_eq!(t.len(), "call_".len() + 8);
    }

    #[test]
    fn conversation_id_is_stable_for_same_messages() {
        let hello = json!("Hello");
        let hi = json!("Hi there!");
        let msgs = vec![
            HashableMessage {
                role: "user",
                content: &hello,
            },
            HashableMessage {
                role: "assistant",
                content: &hi,
            },
        ];
        let a = generate_conversation_id(&msgs);
        let b = generate_conversation_id(&msgs);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn conversation_id_differs_for_different_history() {
        let a_content = json!("Hello");
        let b_content = json!("Goodbye");
        let a = generate_conversation_id(&[HashableMessage {
            role: "user",
            content: &a_content,
        }]);
        let b = generate_conversation_id(&[HashableMessage {
            role: "user",
            content: &b_content,
        }]);
        assert_ne!(a, b);
    }

    #[test]
    fn conversation_id_ignores_middle_messages() {
        let vals: Vec<Value> = (0..6).map(|i| json!(format!("m{i}"))).collect();
        let build = |replace_idx: usize, with: &'static str| -> String {
            let mut owned: Vec<Value> = vals.clone();
            owned[replace_idx] = json!(with);
            let msgs: Vec<HashableMessage> = owned
                .iter()
                .map(|v| HashableMessage {
                    role: "user",
                    content: v,
                })
                .collect();
            generate_conversation_id(&msgs)
        };
        assert_eq!(build(3, "m3"), build(3, "CHANGED"));
        assert_ne!(build(5, "m5"), build(5, "CHANGED"));
    }

    #[test]
    fn empty_messages_yield_random_uuid() {
        let a = generate_conversation_id(&[]);
        let b = generate_conversation_id(&[]);
        assert_ne!(a, b);
        assert_eq!(a.len(), 36, "UUID hyphenated form");
    }

    #[test]
    fn spaced_json_uses_sorted_keys_and_spaced_separators() {
        let v = json!({"role": "user", "content": "hi", "a": 1});
        assert_eq!(
            format_json_spaced_sorted(&v),
            r#"{"a": 1, "content": "hi", "role": "user"}"#
        );
    }

    #[test]
    fn spaced_json_array_separators() {
        let v = json!([1, "two", {"b": 2, "a": 1}]);
        assert_eq!(
            format_json_spaced_sorted(&v),
            r#"[1, "two", {"a": 1, "b": 2}]"#
        );
    }

    #[test]
    fn spaced_json_preserves_document_order() {
        let v: Value = serde_json::from_str(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        assert_eq!(format_json_spaced(&v), r#"{"z": 1, "a": 2, "m": 3}"#);
        assert_eq!(format_json_spaced_sorted(&v), r#"{"a": 2, "m": 3, "z": 1}"#);
    }

    #[test]
    fn spaced_json_is_not_compact() {
        let v = json!({"filePath": "/tmp/x", "limit": 10});
        let out = format_json_spaced(&v);
        assert!(out.contains(", "), "must use ', ' separator");
        assert!(out.contains(": "), "must use ': ' separator");
        assert_ne!(out, serde_json::to_string(&v).unwrap());
    }

    #[test]
    fn truncate_chars_is_codepoint_safe() {
        assert_eq!(truncate_chars("中文测试", 2), "中文");
        assert_eq!(truncate_chars("ab", 100), "ab");
    }

    #[test]
    fn conversation_id_handles_block_array_content() {
        let blocks = json!([{"type": "text", "text": "hello"}]);
        let msgs = vec![HashableMessage {
            role: "user",
            content: &blocks,
        }];
        let id = generate_conversation_id(&msgs);
        assert_eq!(id.len(), 16);
    }
}
