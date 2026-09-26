//! Cache and recovery-prompt helpers for content/tool-call truncation caused
//! by upstream Kiro output-size limits.
//!
//! When Kiro cuts off a streamed response mid-way (a tool call's JSON
//! arguments end unbalanced, or a text response stops without a clean
//! finish), this module records that fact in [`TruncationStore`] — a
//! short-lived, per-conversation, TTL-and-capacity-bounded cache — so a
//! later synthetic follow-up (see [`generate_truncation_tool_result`] /
//! [`generate_truncation_user_message`]) can tell the model what happened
//! instead of silently returning malformed output. Entries are consumed
//! exactly once via the `take_*` methods; the `get_*` methods are thin
//! wrappers kept for call-site clarity but have the same one-shot,
//! removing-on-read semantics.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Synthetic tool-result text injected in place of a truncated tool call, so
/// the model understands the truncation is an API limitation rather than a
/// tool failure and is nudged to change its approach instead of repeating
/// the same (likely-truncated-again) call.
pub const TRUNCATION_TOOL_RESULT_MESSAGE: &str = "[API Limitation] Your tool call was truncated by the upstream API due to output size limits.\n\nIf the tool result below shows an error or unexpected behavior, this is likely a CONSEQUENCE of the truncation, not the root cause. The tool call itself was cut off before it could be fully transmitted.\n\nRepeating the exact same operation will be truncated again. Consider adapting your approach.";
/// System-notice text used when a plain (non-tool-call) response was cut off
/// mid-stream, informing the model/client that the truncation was not an
/// error on the model's part.
pub const TRUNCATION_USER_MESSAGE: &str = "[System Notice] Your previous response was truncated by the API due to output size limitations. This is not an error on your part. If you need to continue, please adapt your approach rather than repeating the same output.";

/// Default time-to-live for cache entries before they are treated as expired
/// and evicted (see [`expire_entries`]).
pub const DEFAULT_TRUNCATION_TTL: Duration = Duration::from_secs(30 * 60);
/// Default maximum number of entries retained in the cache before the
/// least-recently-used entry is evicted.
pub const DEFAULT_TRUNCATION_CACHE_CAPACITY: usize = 1_024;

/// Record of a truncated tool call, keyed by conversation + tool-call id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolTruncationInfo {
    /// The id of the truncated tool call.
    pub tool_call_id: String,
    /// The name of the truncated tool call.
    pub tool_name: String,
    /// Raw diagnostic details (see [`crate::upstream::TruncationInfo`]) as
    /// JSON.
    pub truncation_info: Value,
    /// When this truncation was recorded, used for TTL expiry.
    pub timestamp: SystemTime,
}

/// Record of truncated plain-text content, keyed by conversation + a hash of
/// the (prefix of the) content that was truncated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentTruncationInfo {
    /// Hash of the truncated content's prefix, used to detect whether a
    /// retried request is resending the same truncated content.
    pub message_hash: String,
    /// Human-readable preview of the truncated content.
    pub content_preview: String,
    /// When this truncation was recorded, used for TTL expiry.
    pub timestamp: SystemTime,
}

/// A synthetic `tool_result` block to inject in place of a truncated tool
/// call's real result, produced by [`generate_truncation_tool_result`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticToolResult {
    /// Always `"tool_result"`.
    pub r#type: &'static str,
    /// The id of the tool call this result answers.
    pub tool_use_id: String,
    /// Fixed recovery-notice text explaining the truncation to the model.
    pub content: &'static str,
    /// Always `true` — the synthetic result represents a failure.
    pub is_error: bool,
}

/// Snapshot of how many tool/content truncation entries are currently cached,
/// as returned by [`TruncationStore::stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruncationCacheStats {
    /// Number of cached tool-truncation entries.
    pub tool_truncations: usize,
    /// Number of cached content-truncation entries.
    pub content_truncations: usize,
    /// Sum of `tool_truncations` and `content_truncations`.
    pub total: usize,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum CacheKey {
    Tool {
        conversation_id: String,
        tool_call_id: String,
    },
    Content {
        conversation_id: String,
        message_hash: String,
    },
}

#[derive(Debug, Clone)]
enum CacheValue {
    Tool(ToolTruncationInfo),
    Content(ContentTruncationInfo),
}

#[derive(Debug, Clone)]
struct CacheEntry {
    value: CacheValue,
    last_access: Instant,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
}

/// Thread-safe, TTL-and-capacity-bounded cache of truncation events, scoped
/// per conversation. Cloning shares the same underlying state (it wraps an
/// `Arc<Mutex<_>>`), so a single instance can be held in shared server state
/// (see [`crate::server::AppState::truncation_store`]) and used concurrently
/// across requests.
#[derive(Debug, Clone)]
pub struct TruncationStore {
    state: Arc<Mutex<CacheState>>,
    ttl: Duration,
    capacity: usize,
}

impl Default for TruncationStore {
    fn default() -> Self {
        Self::new(DEFAULT_TRUNCATION_TTL, DEFAULT_TRUNCATION_CACHE_CAPACITY)
    }
}

impl TruncationStore {
    /// Creates a store with a custom TTL and capacity. A `capacity` of `0`
    /// makes the store a no-op: nothing is ever recorded.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::truncation::TruncationStore;
    /// use std::time::Duration;
    ///
    /// let store = TruncationStore::new(Duration::from_secs(60), 100);
    /// assert_eq!(store.stats().total, 0);
    /// ```
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(CacheState::default())),
            ttl,
            capacity,
        }
    }

    /// Records that a tool call was truncated, so a later
    /// [`take_tool_truncation`](Self::take_tool_truncation) can retrieve
    /// `truncation_info` to build a recovery prompt.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::truncation::TruncationStore;
    /// use serde_json::json;
    ///
    /// let store = TruncationStore::default();
    /// store.save_tool_truncation("conv-1", "call_1", "Write", json!({"reason": "cut"}));
    /// assert_eq!(store.stats().tool_truncations, 1);
    /// ```
    pub fn save_tool_truncation(
        &self,
        conversation_id: &str,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        truncation_info: Value,
    ) {
        let tool_call_id = tool_call_id.into();
        let key = CacheKey::Tool {
            conversation_id: conversation_id.to_string(),
            tool_call_id: tool_call_id.clone(),
        };
        let info = ToolTruncationInfo {
            tool_call_id,
            tool_name: tool_name.into(),
            truncation_info,
            timestamp: SystemTime::now(),
        };
        self.insert(key, CacheValue::Tool(info));
    }

    /// Removes and returns the truncation record for `tool_call_id` within
    /// `conversation_id`, if present and not expired. Consumes the entry:
    /// calling this again for the same id returns `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::truncation::TruncationStore;
    /// use serde_json::json;
    ///
    /// let store = TruncationStore::default();
    /// store.save_tool_truncation("conv-1", "call_1", "Write", json!({}));
    /// assert!(store.take_tool_truncation("conv-1", "call_1").is_some());
    /// // The entry is consumed: a second take returns `None`.
    /// assert!(store.take_tool_truncation("conv-1", "call_1").is_none());
    /// ```
    pub fn take_tool_truncation(
        &self,
        conversation_id: &str,
        tool_call_id: &str,
    ) -> Option<ToolTruncationInfo> {
        let key = CacheKey::Tool {
            conversation_id: conversation_id.to_string(),
            tool_call_id: tool_call_id.to_string(),
        };
        self.take(key).and_then(|entry| match entry {
            CacheValue::Tool(info) => Some(info),
            CacheValue::Content(_) => None,
        })
    }

    /// Alias for [`take_tool_truncation`](Self::take_tool_truncation); kept
    /// for call-site readability. Has the same one-shot, removing-on-read
    /// behavior.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::truncation::TruncationStore;
    /// use serde_json::json;
    ///
    /// let store = TruncationStore::default();
    /// store.save_tool_truncation("conv-1", "call_1", "Write", json!({}));
    /// assert!(store.get_tool_truncation("conv-1", "call_1").is_some());
    /// ```
    pub fn get_tool_truncation(
        &self,
        conversation_id: &str,
        tool_call_id: &str,
    ) -> Option<ToolTruncationInfo> {
        self.take_tool_truncation(conversation_id, tool_call_id)
    }

    /// Records that plain-text `content` was truncated, keyed by a hash of
    /// its first 500 characters, and returns that hash so callers can look
    /// it up later via [`take_content_truncation`](Self::take_content_truncation).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::truncation::TruncationStore;
    ///
    /// let store = TruncationStore::default();
    /// let hash = store.save_content_truncation("conv-1", "cut off mid-sent");
    /// assert_eq!(hash.len(), 16);
    /// ```
    pub fn save_content_truncation(&self, conversation_id: &str, content: &str) -> String {
        let message_hash = content_hash(content);
        let key = CacheKey::Content {
            conversation_id: conversation_id.to_string(),
            message_hash: message_hash.clone(),
        };
        let info = ContentTruncationInfo {
            message_hash: message_hash.clone(),
            content_preview: prefix_chars(content, 200),
            timestamp: SystemTime::now(),
        };
        self.insert(key, CacheValue::Content(info));
        message_hash
    }

    /// Removes and returns the truncation record matching `content`'s hash
    /// within `conversation_id`, if present and not expired. Consumes the
    /// entry: calling this again for the same content returns `None`.
    pub fn take_content_truncation(
        &self,
        conversation_id: &str,
        content: &str,
    ) -> Option<ContentTruncationInfo> {
        let key = CacheKey::Content {
            conversation_id: conversation_id.to_string(),
            message_hash: content_hash(content),
        };
        self.take(key).and_then(|entry| match entry {
            CacheValue::Content(info) => Some(info),
            CacheValue::Tool(_) => None,
        })
    }

    /// Alias for [`take_content_truncation`](Self::take_content_truncation);
    /// kept for call-site readability. Has the same one-shot,
    /// removing-on-read behavior.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::truncation::TruncationStore;
    ///
    /// let store = TruncationStore::default();
    /// store.save_content_truncation("conv-1", "cut off");
    /// assert!(store.get_content_truncation("conv-1", "cut off").is_some());
    /// ```
    pub fn get_content_truncation(
        &self,
        conversation_id: &str,
        content: &str,
    ) -> Option<ContentTruncationInfo> {
        self.take_content_truncation(conversation_id, content)
    }

    /// Returns current entry counts after first evicting any expired
    /// entries (see [`expire_entries`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::truncation::TruncationStore;
    ///
    /// let store = TruncationStore::default();
    /// store.save_content_truncation("conv-1", "cut off");
    /// let stats = store.stats();
    /// assert_eq!(stats.content_truncations, 1);
    /// assert_eq!(stats.total, 1);
    /// ```
    pub fn stats(&self) -> TruncationCacheStats {
        let mut state = self.lock_state();
        expire_entries(&mut state, self.ttl);
        let tool_truncations = state
            .entries
            .values()
            .filter(|entry| matches!(entry.value, CacheValue::Tool(_)))
            .count();
        let content_truncations = state.entries.len().saturating_sub(tool_truncations);
        TruncationCacheStats {
            tool_truncations,
            content_truncations,
            total: state.entries.len(),
        }
    }

    // Inserts an entry after expiring stale ones, then evicts the
    // least-recently-used entry (repeatedly, though normally at most once)
    // until the cache is back within `capacity`. A `capacity` of 0 is a
    // deliberate no-op: nothing is ever inserted.
    fn insert(&self, key: CacheKey, value: CacheValue) {
        let mut state = self.lock_state();
        expire_entries(&mut state, self.ttl);
        if self.capacity == 0 {
            return;
        }
        state.entries.insert(
            key,
            CacheEntry {
                value,
                last_access: Instant::now(),
            },
        );
        while state.entries.len() > self.capacity {
            if let Some(oldest_key) = least_recently_used_key(&state) {
                state.entries.remove(&oldest_key);
            } else {
                break;
            }
        }
    }

    // Removes and returns an entry by key after first expiring stale ones,
    // implementing the one-shot "take" semantics shared by all public
    // lookup methods.
    fn take(&self, key: CacheKey) -> Option<CacheValue> {
        let mut state = self.lock_state();
        expire_entries(&mut state, self.ttl);
        state.entries.remove(&key).map(|entry| entry.value)
    }

    // Locks the internal state, recovering from mutex poisoning (a prior
    // panic while holding the lock) rather than propagating a panic here,
    // since a poisoned truncation cache is not worth crashing the whole
    // gateway over.
    fn lock_state(&self) -> MutexGuard<'_, CacheState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("truncation cache lock was poisoned; recovering state");
                poisoned.into_inner()
            }
        }
    }
}

/// Heuristic for whether a completed stream represents truncated content:
/// true when the stream did not finish normally, some content was actually
/// produced, and it wasn't a tool call (tool-call truncation is diagnosed
/// separately via [`crate::upstream::diagnose_json_truncation`]).
///
/// # Examples
///
/// ```
/// use lanius_core::truncation::is_content_truncated;
///
/// assert!(is_content_truncated(false, "partial output", false));
/// assert!(!is_content_truncated(true, "partial output", false));
/// ```
pub fn is_content_truncated(
    stream_completed_normally: bool,
    full_content: &str,
    has_tool_calls: bool,
) -> bool {
    !stream_completed_normally && !full_content.is_empty() && !has_tool_calls
}

/// Builds a synthetic, error-flagged tool result explaining that this tool
/// call was truncated by the upstream API, for injection into the
/// conversation history in place of (or alongside) the real result.
///
/// # Examples
///
/// ```
/// use lanius_core::truncation::generate_truncation_tool_result;
///
/// let result = generate_truncation_tool_result("toolu_1");
/// assert_eq!(result.tool_use_id, "toolu_1");
/// assert!(result.is_error);
/// ```
pub fn generate_truncation_tool_result(tool_use_id: impl Into<String>) -> SyntheticToolResult {
    SyntheticToolResult {
        r#type: "tool_result",
        tool_use_id: tool_use_id.into(),
        content: TRUNCATION_TOOL_RESULT_MESSAGE,
        is_error: true,
    }
}

/// Returns the static system-notice text used when a plain-text response was
/// truncated mid-stream.
///
/// # Examples
///
/// ```
/// use lanius_core::truncation::generate_truncation_user_message;
///
/// assert!(generate_truncation_user_message().contains("truncated"));
/// ```
pub fn generate_truncation_user_message() -> &'static str {
    TRUNCATION_USER_MESSAGE
}

/// Prepends the standard truncation notice to an original (real) tool result
/// string, preserving the original result for context while still flagging
/// that it was affected by truncation.
///
/// # Examples
///
/// ```
/// use lanius_core::truncation::prepend_tool_recovery_notice;
///
/// let notice = prepend_tool_recovery_notice("file not found");
/// assert!(notice.ends_with("file not found"));
/// ```
pub fn prepend_tool_recovery_notice(original_tool_result: &str) -> String {
    format!(
        "{TRUNCATION_TOOL_RESULT_MESSAGE}\n\n---\n\nOriginal tool result:\n{original_tool_result}"
    )
}

// Hashes the first 500 characters of `content` with SHA-256 and truncates
// the hex digest to 16 characters; a prefix-based hash (rather than the
// whole string) keeps this cheap for very large truncated responses while
// still being effectively unique per distinct truncation event.
fn content_hash(content: &str) -> String {
    let prefix = prefix_chars(content, 500);
    let digest = Sha256::digest(prefix.as_bytes());
    hex::encode(digest)[..16].to_string()
}

// Takes the first `limit` Unicode scalar values of `text`, safe for
// multi-byte characters (unlike naive byte slicing).
fn prefix_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

// Evicts entries whose `last_access` is older than `ttl`; called at the
// start of every cache operation so expiry is enforced lazily rather than
// via a background sweeper task.
fn expire_entries(state: &mut CacheState, ttl: Duration) {
    state
        .entries
        .retain(|_, entry| entry.last_access.elapsed() <= ttl);
}

// Finds the key of the entry with the oldest `last_access` time, used by
// `insert` to implement LRU eviction once the cache exceeds its capacity.
fn least_recently_used_key(state: &CacheState) -> Option<CacheKey> {
    state
        .entries
        .iter()
        .min_by_key(|(_, entry)| entry.last_access)
        .map(|(key, _)| key.clone())
}

#[cfg(test)]
mod tests {
    use std::thread;

    use serde_json::json;

    use super::*;

    #[test]
    fn tool_entries_are_conversation_scoped_and_one_shot() {
        let store = TruncationStore::default();
        store.save_tool_truncation("conversation-a", "call_1", "Write", json!({"reason":"cut"}));
        assert!(
            store
                .take_tool_truncation("conversation-b", "call_1")
                .is_none()
        );
        let entry = store.take_tool_truncation("conversation-a", "call_1");
        assert_eq!(
            entry.as_ref().map(|entry| entry.tool_name.as_str()),
            Some("Write")
        );
        assert!(
            store
                .take_tool_truncation("conversation-a", "call_1")
                .is_none()
        );
    }

    #[test]
    fn content_hash_and_preview_are_unicode_safe_and_one_shot() {
        let store = TruncationStore::default();
        let content = format!("{} tail", "你".repeat(600));
        let hash = store.save_content_truncation("conversation", &content);
        assert_eq!(hash.len(), 16);
        let entry = store.take_content_truncation("conversation", &content);
        assert_eq!(
            entry
                .as_ref()
                .map(|entry| entry.content_preview.chars().count()),
            Some(200)
        );
        assert!(
            store
                .take_content_truncation("conversation", &content)
                .is_none()
        );
    }

    #[test]
    fn ttl_expires_abandoned_entries() {
        let store = TruncationStore::new(Duration::from_millis(1), 10);
        store.save_content_truncation("conversation", "cut off");
        thread::sleep(Duration::from_millis(5));
        assert!(
            store
                .take_content_truncation("conversation", "cut off")
                .is_none()
        );
        assert_eq!(store.stats().total, 0);
    }

    #[test]
    fn lru_capacity_evicts_oldest_entry_and_replacing_key_refreshes_it() {
        let store = TruncationStore::new(Duration::from_secs(60), 2);
        store.save_content_truncation("conversation", "first");
        thread::sleep(Duration::from_millis(1));
        store.save_content_truncation("conversation", "second");
        thread::sleep(Duration::from_millis(1));
        store.save_content_truncation("conversation", "first");
        thread::sleep(Duration::from_millis(1));
        store.save_content_truncation("conversation", "third");
        assert!(
            store
                .take_content_truncation("conversation", "first")
                .is_some()
        );
        assert!(
            store
                .take_content_truncation("conversation", "second")
                .is_none()
        );
        assert!(
            store
                .take_content_truncation("conversation", "third")
                .is_some()
        );
    }

    #[test]
    fn capacity_zero_never_records_state() {
        let store = TruncationStore::new(Duration::from_secs(60), 0);
        store.save_tool_truncation("conversation", "call", "tool", json!({}));
        assert_eq!(store.stats().total, 0);
    }

    #[test]
    fn detection_and_recovery_messages_match_expected_literals() {
        assert!(is_content_truncated(false, "partial output", false));
        assert!(!is_content_truncated(true, "partial output", false));
        assert!(!is_content_truncated(false, "", false));
        assert!(!is_content_truncated(false, "partial output", true));
        assert_eq!(generate_truncation_user_message(), TRUNCATION_USER_MESSAGE);
        let result = generate_truncation_tool_result("toolu_1");
        assert_eq!(result.r#type, "tool_result");
        assert!(result.is_error);
        assert_eq!(result.content, TRUNCATION_TOOL_RESULT_MESSAGE);
        assert_eq!(
            prepend_tool_recovery_notice("failed"),
            format!("{TRUNCATION_TOOL_RESULT_MESSAGE}\n\n---\n\nOriginal tool result:\nfailed")
        );
    }
}
