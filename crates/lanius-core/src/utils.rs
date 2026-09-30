//! Fingerprinting, user-agent, id generation, and spaced-JSON helpers.
//!
//! Kiro's backend expects requests shaped exactly like an official client's.
//! Lanius identifies as the Kiro CLI ([`kiro_headers`], [`kiro_user_agent`]),
//! keeps a stable per-machine fingerprint ([`machine_fingerprint`]) for
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

// Client identity mirrored from the official Kiro CLI (captured from
// kiro-cli 2.24.0 request logs). Bump these together when Kiro raises its
// minimum supported client version.
const KIRO_CLI_VERSION: &str = "2.24.0";
const AWS_SDK_RUST_VERSION: &str = "1.3.15";
const RUST_VERSION: &str = "1.92.0";
const KIRO_CLI_APP: &str = "app/AmazonQ-For-CLI";

/// Smithy service identifier for the streaming chat API (`GenerateAssistantResponse`).
pub const API_STREAMING: &str = "codewhispererstreaming/0.1.17975";
/// Smithy service identifier for the non-streaming runtime API (profiles, models, usage).
pub const API_RUNTIME: &str = "codewhispererruntime/0.1.17975";
/// Smithy service identifier for the AWS SSO OIDC token API.
pub const API_SSO_OIDC: &str = "ssooidc/1.100.0";

/// `origin` value the Kiro CLI sends in chat payloads and runtime API bodies.
pub const KIRO_ORIGIN: &str = "KIRO_CLI";

/// `User-Agent` value the Kiro CLI sends on social-login token refresh.
pub const KIRO_CLI_REFRESH_USER_AGENT: &str = "Kiro-CLI";

/// The Kiro CLI's short OS tag (`macos`, `linux`, `windows`), matching both the
/// `os/` user-agent component and `envState.operatingSystem`.
///
/// # Examples
///
/// ```
/// let os = lanius_core::utils::kiro_os();
/// assert!(!os.is_empty());
/// ```
pub fn kiro_os() -> &'static str {
    std::env::consts::OS
}

/// Kiro CLI `User-Agent` for a Kiro-specific smithy client identified by `api`.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::{API_STREAMING, kiro_user_agent};
///
/// let ua = kiro_user_agent(API_STREAMING);
/// assert!(ua.starts_with("aws-sdk-rust/"));
/// assert!(ua.ends_with("app/AmazonQ-For-CLI"));
/// ```
pub fn kiro_user_agent(api: &str) -> String {
    format!(
        "aws-sdk-rust/{AWS_SDK_RUST_VERSION} ua/2.1 api/{api} os/{os} lang/rust/{RUST_VERSION} \
         md/appVersion-{KIRO_CLI_VERSION} {KIRO_CLI_APP}",
        os = kiro_os()
    )
}

/// Kiro CLI `x-amz-user-agent` for a Kiro-specific smithy client identified by `api`.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::{API_RUNTIME, kiro_amz_user_agent};
///
/// assert!(kiro_amz_user_agent(API_RUNTIME).contains(" m/F "));
/// ```
pub fn kiro_amz_user_agent(api: &str) -> String {
    format!(
        "aws-sdk-rust/{AWS_SDK_RUST_VERSION} ua/2.1 api/{api} os/{os} lang/rust/{RUST_VERSION} \
         m/F {KIRO_CLI_APP}",
        os = kiro_os()
    )
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
    let os = kiro_os();
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
/// `amz-sdk-invocation-id` per call. `x-amz-target` is added by callers.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::{API_RUNTIME, kiro_headers_for};
///
/// let headers = kiro_headers_for("tok", API_RUNTIME);
/// assert!(headers.iter().any(|(k, v)| *k == "User-Agent" && v.contains("codewhispererruntime")));
/// ```
pub fn kiro_headers_for(token: &str, api: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("Content-Type", "application/x-amz-json-1.0".to_string()),
        ("User-Agent", kiro_user_agent(api)),
        ("x-amz-user-agent", kiro_amz_user_agent(api)),
        ("x-amzn-codewhisperer-optout", "false".to_string()),
        ("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string()),
        ("amz-sdk-request", "attempt=1; max=3".to_string()),
    ]
}

/// Builds the full set of headers for a Kiro `GenerateAssistantResponse`
/// request, matching what the Kiro CLI sends to the streaming API.
///
/// # Examples
///
/// ```
/// use lanius_core::utils::kiro_headers;
///
/// let headers = kiro_headers("my-token");
/// assert!(headers.iter().any(|(k, v)| *k == "Authorization" && v == "Bearer my-token"));
/// assert!(headers.iter().any(|(k, _)| *k == "x-amz-user-agent"));
/// ```
pub fn kiro_headers(token: &str) -> Vec<(&'static str, String)> {
    let mut headers = kiro_headers_for(token, API_STREAMING);
    headers.push((
        "x-amz-target",
        "AmazonCodeWhispererStreamingService.GenerateAssistantResponse".to_string(),
    ));
    headers.push(("x-kiro-attempt", "1;max=3".to_string()));
    headers
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
        assert_eq!(
            kiro_user_agent(API_STREAMING),
            format!(
                "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.17975 os/{os} \
                 lang/rust/1.92.0 md/appVersion-2.24.0 app/AmazonQ-For-CLI"
            )
        );
        assert_eq!(
            kiro_amz_user_agent(API_RUNTIME),
            format!(
                "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererruntime/0.1.17975 os/{os} \
                 lang/rust/1.92.0 m/F app/AmazonQ-For-CLI"
            )
        );
        assert_eq!(
            aws_sdk_user_agents(API_SSO_OIDC),
            (
                format!("aws-sdk-rust/1.3.15 os/{os} lang/rust/1.92.0"),
                format!(
                    "aws-sdk-rust/1.3.15 ua/2.1 api/ssooidc/1.100.0 os/{os} lang/rust/1.92.0 \
                     app/AmazonQ-For-CLI"
                ),
            )
        );
    }

    #[test]
    fn headers_never_identify_as_kiro_ide() {
        for (name, value) in kiro_headers("tok") {
            assert!(!value.contains("KiroIDE"), "{name} still claims KiroIDE");
            assert!(
                !value.contains("aws-sdk-js"),
                "{name} still claims the JS SDK"
            );
        }
        assert_eq!(kiro_os(), std::env::consts::OS);
    }

    #[test]
    fn header_set_is_complete() {
        let headers = kiro_headers("tok");
        let names: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
        for expected in [
            "Authorization",
            "Content-Type",
            "x-amz-target",
            "User-Agent",
            "x-amz-user-agent",
            "x-amzn-codewhisperer-optout",
            "x-kiro-attempt",
            "amz-sdk-invocation-id",
            "amz-sdk-request",
        ] {
            assert!(names.contains(&expected), "missing header {expected}");
        }
        let auth = &headers
            .iter()
            .find(|(k, _)| *k == "Authorization")
            .unwrap()
            .1;
        assert_eq!(auth, "Bearer tok");
        let ct = &headers
            .iter()
            .find(|(k, _)| *k == "Content-Type")
            .unwrap()
            .1;
        assert_eq!(ct, "application/x-amz-json-1.0");
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
