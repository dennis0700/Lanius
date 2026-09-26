//! Protective post-processing for a fully-built Kiro request payload.
//!
//! Kiro imposes a maximum request size, and its conversation history format requires every
//! `toolResults` entry to reference a `toolUseId` that actually appeared in the
//! immediately preceding assistant turn's `toolUses`. When [`super::core::build_kiro_payload`]
//! needs to shrink an oversized payload by dropping older history entries, that dropping
//! can easily produce exactly the kind of dangling reference Kiro would reject. This module
//! provides:
//!
//! - [`check_payload_size`] — measures a payload's size the same way Kiro does: as compact
//!   ASCII-escaped JSON (see [`compact_ascii_json`]), so multi-byte characters are counted
//!   by their `\uXXXX`-escaped length rather than their raw UTF-8 byte length.
//! - [`trim_payload_to_limit`] — the trimming algorithm itself: strips harmless empty
//!   `toolUses` arrays, then repeatedly drops the *oldest two* history entries (a
//!   user/assistant pair) until the payload fits or no more can safely be dropped, then
//!   repairs any tool results left dangling by the trim (see
//!   [`repair_orphaned_tool_results`]) and drops any leading entries that no longer start
//!   with a `userInputMessage` (Kiro requires history to start on a user turn).

use serde_json::{Map, Value};

/// Before/after size and entry-count summary returned by [`trim_payload_to_limit`], so
/// callers can log/observe how much (if anything) was trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadTrimStats {
    /// Payload size in bytes (per [`check_payload_size`]) before trimming.
    pub original_bytes: usize,
    /// Payload size in bytes (per [`check_payload_size`]) after trimming.
    pub final_bytes: usize,
    /// Number of `conversationState.history` entries before trimming.
    pub original_entries: usize,
    /// Number of `conversationState.history` entries after trimming.
    pub final_entries: usize,
    /// Whether any entries were actually removed.
    pub trimmed: bool,
}

/// Measures `payload`'s size exactly as Kiro would count it: the length, in bytes, of its
/// compact ASCII-escaped JSON serialization (see [`compact_ascii_json`]). This is
/// deliberately *not* `serde_json::to_string(payload).len()`, since that would count
/// multi-byte UTF-8 characters by their raw byte length rather than their `\uXXXX`-escaped
/// length, undercounting the size of payloads containing non-ASCII text.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::check_payload_size;
/// use serde_json::json;
///
/// let payload = json!({"text": "hi"});
/// assert_eq!(check_payload_size(&payload), r#"{"text":"hi"}"#.len());
/// ```
pub fn check_payload_size(payload: &Value) -> usize {
    compact_ascii_json(payload).len()
}

/// Trims `payload`'s `conversationState.history` array down towards `max_bytes`, then
/// repairs the result so it stays valid for Kiro to accept.
///
/// Algorithm:
/// 1. If there is no history array, or it is empty, there is nothing to trim — return
///    stats reporting zero entries and no change.
/// 2. Strip any assistant turn's `toolUses` field when it is present but empty — a
///    harmless simplification that also slightly reduces size before the main loop.
/// 3. Repeatedly drop the *oldest two* history entries (`history.drain(..2)`) — two at a
///    time, not one, because Kiro's history alternates user/assistant turns and dropping
///    only one would misalign that pairing — as long as more than 2 entries remain *and*
///    the payload is still over `max_bytes`. This preserves at least the most recent
///    exchange rather than trimming down to nothing.
/// 4. After trimming, drop any remaining leading entries that are not `userInputMessage`
///    turns (Kiro requires history to begin on a user turn — dropping in pairs alone does
///    not always land on a user-first boundary, e.g. if the original history had an
///    irregular shape).
/// 5. Repair any `toolResults` entries left referencing a `toolUseId` that no longer has a
///    matching `toolUses` entry in the now-preceding turn (see
///    [`repair_orphaned_tool_results`]) — trimming can and does create these, since a tool
///    call's originating assistant turn may have been trimmed away while a later user turn
///    still references it.
///
/// # Examples
///
/// ```
/// use lanius_core::convert::trim_payload_to_limit;
/// use serde_json::json;
///
/// let mut payload = json!({"conversationState": {"history": [
///     {"userInputMessage": {"content": "old"}},
///     {"assistantResponseMessage": {"content": "reply"}},
///     {"userInputMessage": {"content": "new"}},
/// ]}});
/// let stats = trim_payload_to_limit(&mut payload, 10);
/// assert!(stats.trimmed);
/// ```
pub fn trim_payload_to_limit(payload: &mut Value, max_bytes: usize) -> PayloadTrimStats {
    let original_bytes = check_payload_size(payload);
    let original_entries = match history_mut(payload) {
        Some(history) if !history.is_empty() => history.len(),
        _ => {
            return PayloadTrimStats {
                original_bytes,
                final_bytes: original_bytes,
                original_entries: 0,
                final_entries: 0,
                trimmed: false,
            };
        }
    };
    if let Some(history) = history_mut(payload) {
        strip_empty_tool_uses(history);
    }
    loop {
        let can_trim = history_mut(payload).is_some_and(|history| history.len() > 2);
        if !can_trim || check_payload_size(payload) <= max_bytes {
            break;
        }
        // Drop the oldest user/assistant pair together to keep alternation intact.
        if let Some(history) = history_mut(payload) {
            history.drain(..2);
        }
    }
    if let Some(history) = history_mut(payload) {
        while history
            .first()
            .and_then(Value::as_object)
            .is_some_and(|entry| !entry.contains_key("userInputMessage"))
        {
            history.remove(0);
        }
        repair_orphaned_tool_results(history);
    }
    let final_entries = history_mut(payload).map_or(0, |history| history.len());
    PayloadTrimStats {
        original_bytes,
        final_bytes: check_payload_size(payload),
        original_entries,
        final_entries,
        trimmed: original_entries != final_entries,
    }
}

fn history_mut(payload: &mut Value) -> Option<&mut Vec<Value>> {
    payload
        .get_mut("conversationState")?
        .get_mut("history")?
        .as_array_mut()
}

/// Removes an assistant turn's `toolUses` field when it is present but an empty array —
/// Kiro treats an absent field and an empty array equivalently, so dropping it is a pure
/// size reduction with no behavioral difference.
fn strip_empty_tool_uses(history: &mut [Value]) {
    for entry in history {
        if let Some(assistant) = entry
            .get_mut("assistantResponseMessage")
            .and_then(Value::as_object_mut)
        {
            if assistant.get("toolUses").is_some_and(Value::is_array)
                && assistant
                    .get("toolUses")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
            {
                assistant.remove("toolUses");
            }
        }
    }
}

/// Scans `history` in order and, for each `userInputMessage` entry, removes any
/// `toolResults` entry whose `toolUseId` does not appear among the `toolUses` of the
/// *immediately preceding* history entry (the assistant turn that would have produced it).
/// Kiro requires this pairing to hold, and trimming (dropping older entries) can easily
/// break it by removing the assistant turn that originated a tool call while leaving a
/// later user turn's tool result referencing it.
///
/// Removed tool results are not silently discarded: their text content is collected (via
/// [`collect_tool_text`]) and appended to the user message's own `content` as a
/// `[trimmed tool result] ...` note, so the model still sees that a result existed even
/// though the structured tool-result linkage had to be dropped. If removing entries leaves
/// `userInputMessageContext` empty, that now-empty context object is removed entirely
/// rather than left as clutter.
fn repair_orphaned_tool_results(history: &mut [Value]) {
    for index in 0..history.len() {
        let valid_ids = if index == 0 {
            std::collections::HashSet::new()
        } else {
            history[index - 1]
                .get("assistantResponseMessage")
                .and_then(|assistant| assistant.get("toolUses"))
                .and_then(Value::as_array)
                .map(|uses| {
                    uses.iter()
                        .filter_map(|tool_use| {
                            tool_use
                                .get("toolUseId")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let Some(user) = history[index]
            .get_mut("userInputMessage")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let original_content = user
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let (changed, context_empty, orphan_text) = {
            let Some(context) = user
                .get_mut("userInputMessageContext")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            let Some(results) = context.get_mut("toolResults").and_then(Value::as_array_mut) else {
                continue;
            };
            let old_len = results.len();
            let mut orphan_text = Vec::new();
            results.retain(|result| {
                let valid = result
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| valid_ids.contains(id));
                if !valid {
                    collect_tool_text(result.get("content"), &mut orphan_text);
                }
                valid
            });
            let changed = results.len() != old_len;
            if results.is_empty() {
                context.remove("toolResults");
            }
            (changed, context.is_empty(), orphan_text)
        };
        if changed && !orphan_text.is_empty() {
            user.insert(
                "content".to_owned(),
                Value::String(format!(
                    "{original_content}\n[trimmed tool result] {}",
                    orphan_text.join("; ")
                )),
            );
        }
        if context_empty {
            user.remove("userInputMessageContext");
        }
    }
}

/// Extracts human-readable text from a tool result's `content` field (either a plain
/// string, or an array of `{"text": ...}`-shaped blocks), appending non-empty pieces to
/// `output`. Used by [`repair_orphaned_tool_results`] to preserve a trace of what an
/// orphaned tool result said, even after the structured result itself is dropped.
fn collect_tool_text(content: Option<&Value>, output: &mut Vec<String>) {
    match content {
        Some(Value::String(text)) if !text.is_empty() => output.push(text.clone()),
        Some(Value::Array(parts)) => output.extend(parts.iter().filter_map(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        })),
        _ => {}
    }
}

/// Serializes `value` to compact JSON using [`write_compact_ascii`] — no extra whitespace,
/// and every non-ASCII character escaped as `\uXXXX` (or a surrogate pair for characters
/// outside the Basic Multilingual Plane) — matching the exact byte-counting convention
/// Kiro's own size limit uses (see [`check_payload_size`]).
pub(crate) fn compact_ascii_json(value: &Value) -> String {
    let mut output = String::new();
    write_compact_ascii(value, &mut output);
    output
}

fn write_compact_ascii(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(boolean) => output.push_str(if *boolean { "true" } else { "false" }),
        Value::Number(number) => output.push_str(&number.to_string()),
        Value::String(string) => write_ascii_string(string, output),
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_compact_ascii(item, output);
            }
            output.push(']');
        }
        Value::Object(map) => write_object(map, output),
    }
}

fn write_object(map: &Map<String, Value>, output: &mut String) {
    output.push('{');
    for (index, (key, value)) in map.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write_ascii_string(key, output);
        output.push(':');
        write_compact_ascii(value, output);
    }
    output.push('}');
}

/// Writes `string` as a JSON string literal with every byte kept ASCII: standard escapes
/// for quote/backslash/control characters, and every other non-ASCII or non-printable
/// character emitted as `\uXXXX` (splitting characters above the Basic Multilingual Plane
/// into a UTF-16 surrogate pair, matching how `JSON.stringify`/most JSON encoders with
/// "ensure_ascii" behave).
fn write_ascii_string(string: &str, output: &mut String) {
    output.push('"');
    for character in string.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0C}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1F}' => {
                let _ =
                    std::fmt::Write::write_fmt(output, format_args!("\\u{:04x}", character as u32));
            }
            character if character.is_ascii() => output.push(character),
            character if (character as u32) <= 0xffff => {
                let _ =
                    std::fmt::Write::write_fmt(output, format_args!("\\u{:04x}", character as u32));
            }
            character => {
                let code = character as u32 - 0x1_0000;
                let high = 0xd800 + (code >> 10);
                let low = 0xdc00 + (code & 0x3ff);
                let _ =
                    std::fmt::Write::write_fmt(output, format_args!("\\u{high:04x}\\u{low:04x}"));
            }
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn size_is_compact_ascii_utf8_bytes() {
        let payload = json!({"chinese":"你好", "emoji":"😀"});
        assert_eq!(
            compact_ascii_json(&payload),
            r#"{"chinese":"\u4f60\u597d","emoji":"\ud83d\ude00"}"#
        );
        assert_eq!(
            check_payload_size(&payload),
            compact_ascii_json(&payload).len()
        );
    }

    #[test]
    fn trims_pairs_aligns_and_repairs_orphans() {
        let mut payload = json!({"conversationState":{"history":[
            {"assistantResponseMessage":{"content":"bad start","toolUses":[]}},
            {"userInputMessage":{"content":"old"}},
            {"assistantResponseMessage":{"content":"call","toolUses":[{"toolUseId":"ok"}]}},
            {"userInputMessage":{"content":"new","userInputMessageContext":{"toolResults":[{"toolUseId":"orphan","content":[{"text":"keep me"}]}]}}},
            {"assistantResponseMessage":{"content":"tail"}}
        ]}});
        let max_bytes = check_payload_size(&payload) + 100;
        let stats = trim_payload_to_limit(&mut payload, max_bytes);
        let history = payload["conversationState"]["history"].as_array().unwrap();
        assert!(history.is_empty() || history[0].get("userInputMessage").is_some());
        assert!(stats.trimmed);
        let repaired = history
            .iter()
            .filter_map(|entry| entry.get("userInputMessage"))
            .find(|user| {
                user["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("keep me")
            });
        assert!(repaired.is_some());
    }
}
