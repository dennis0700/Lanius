//! Decodes Kiro's raw upstream response bytes into structured events.
//!
//! Kiro streams its response as a sequence of small JSON objects
//! concatenated back-to-back inside an AWS event-stream frame (this module
//! does not decode the full AWS event-stream binary framing — it works
//! directly on the embedded JSON fragments by pattern-matching known key
//! prefixes). [`AwsEventStreamParser`] is the stateful, incremental
//! decoder: feed it raw bytes as they arrive (safely handling UTF-8
//! sequences and JSON objects split across chunk boundaries) and it emits
//! [`ParserEvent`]s for content/native reasoning/usage/context-usage, while accumulating
//! multi-chunk tool-call fragments internally and exposing them via
//! [`AwsEventStreamParser::take_tool_calls`]. Separately,
//! [`parse_bracket_tool_calls`] recognizes an older, non-JSON tool-call
//! convention (`[Called <name> with args: {...}]`) embedded directly in
//! text content. [`crate::upstream::stream`] is the only consumer of both.

use serde_json::Value;

use crate::utils::{format_json_spaced, generate_tool_call_id};

/// Finds the index of the `}` that closes the JSON object starting at
/// `start` (which must point at a `{`), by tracking brace depth while
/// correctly skipping over `{`/`}` characters that appear inside string
/// literals (respecting `\"`-escaped quotes so an escaped quote doesn't
/// prematurely end string-tracking). Returns `None` if `start` isn't a `{`
/// or the object is never closed within `text`. Operates on bytes, but only
/// treats ASCII `{`, `}`, `"`, and `\\` specially, so it never splits a
/// multi-byte UTF-8 character (those bytes are always `>= 0x80` and fall
/// through the `else` branches unchanged).
///
/// # Examples
///
/// ```
/// use lanius_core::upstream::find_matching_brace;
///
/// let text = r#"{"a": {"nested": 1}}"#;
/// assert_eq!(find_matching_brace(text, 0), Some(text.len() - 1));
/// ```
pub fn find_matching_brace(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if start >= bytes.len() || bytes[start] != b'{' {
        return None;
    }

    let mut brace_count: i32 = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if escape_next {
            escape_next = false;
            continue;
        }
        if c == b'\\' && in_string {
            escape_next = true;
            continue;
        }
        if c == b'"' {
            in_string = !in_string;
            continue;
        }
        if !in_string {
            if c == b'{' {
                brace_count += 1;
            } else if c == b'}' {
                brace_count -= 1;
                if brace_count == 0 {
                    return Some(i);
                }
            }
        }
    }

    None
}

/// Diagnostic result from [`diagnose_json_truncation`], explaining why a
/// tool call's raw argument JSON failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationInfo {
    /// Whether the JSON appears to have been cut off mid-stream (as opposed
    /// to being malformed for some other reason).
    pub is_truncated: bool,
    /// Human-readable explanation (e.g. "missing 2 closing brace(s)").
    pub reason: String,
    /// Length of the raw string that was diagnosed, in bytes.
    pub size_bytes: usize,
}

/// A single tool call extracted from the upstream response, either from
/// structured event fields (see [`AwsEventStreamParser`]) or from the
/// bracket-style text convention (see [`parse_bracket_tool_calls`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The tool call's id, when the upstream provided one. Absent ids are
    /// deduplicated/ordered specially — see [`deduplicate_tool_calls`].
    pub id: Option<String>,
    /// The name of the tool being called.
    pub name: String,
    /// JSON-encoded arguments, formatted with spaced (`", "`/`": "`)
    /// separators via [`format_json_spaced`].
    pub arguments: String,
    /// Set when `arguments` had to be replaced with `"{}"` because the raw
    /// JSON was truncated (see [`diagnose_json_truncation`]).
    pub truncation: Option<TruncationInfo>,
}

/// Deduplicates a list of tool calls that may contain repeated/partial
/// entries for the same logical call, matching the priorities Kiro's
/// streaming protocol requires:
///
/// 1. Calls that share a non-empty `id` are merged into one, keeping
///    whichever version has "better" arguments — non-`"{}"` wins over
///    `"{}"`, and otherwise the longer argument string wins (a proxy for
///    "more complete", since later chunks tend to add fields).
/// 2. After id-based merging, calls with an id are ordered before calls
///    without one, preserving each group's original relative order.
/// 3. Finally, any remaining calls with identical `name` + `arguments` are
///    collapsed to a single entry (even across different ids), since that
///    combination is very unlikely to represent two genuinely different
///    calls.
///
/// # Examples
///
/// ```
/// use lanius_core::upstream::{deduplicate_tool_calls, ToolCall};
///
/// let calls = vec![
///     ToolCall { id: Some("1".into()), name: "read".into(), arguments: "{}".into(), truncation: None },
///     ToolCall { id: Some("1".into()), name: "read".into(), arguments: r#"{"path":"x"}"#.into(), truncation: None },
/// ];
/// let deduped = deduplicate_tool_calls(calls);
/// assert_eq!(deduped.len(), 1);
/// assert_eq!(deduped[0].arguments, r#"{"path":"x"}"#);
/// ```
pub fn deduplicate_tool_calls(tool_calls: Vec<ToolCall>) -> Vec<ToolCall> {
    let mut id_order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, ToolCall> = std::collections::HashMap::new();
    let mut without_id: Vec<ToolCall> = Vec::new();

    for tc in tool_calls {
        let Some(id) = tc.id.clone().filter(|id| !id.is_empty()) else {
            without_id.push(tc);
            continue;
        };
        match by_id.get(&id) {
            None => {
                id_order.push(id.clone());
                by_id.insert(id, tc);
            }
            Some(existing) => {
                let current_args = &tc.arguments;
                let existing_args = &existing.arguments;
                // Prefer whichever version looks more complete: a non-empty
                // object always beats an empty one, and otherwise the
                // longer string is assumed to carry more fields.
                if current_args != "{}"
                    && (existing_args == "{}" || current_args.len() > existing_args.len())
                {
                    tracing::debug!(
                        tool_id = %id,
                        from = existing_args.len(),
                        to = current_args.len(),
                        "replacing tool call with better arguments"
                    );
                    by_id.insert(id, tc);
                }
            }
        }
    }

    let mut ordered: Vec<ToolCall> = id_order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect();
    ordered.extend(without_id);

    // Final pass: collapse any remaining calls that are identical in every
    // way that matters (name + arguments), regardless of id, since such
    // duplicates are never intentionally distinct calls.
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(ordered.len());
    for tc in ordered {
        let key = format!("{}-{}", tc.name, tc.arguments);
        if seen.insert(key) {
            unique.push(tc);
        }
    }

    unique
}

/// Extracts tool calls written using the older `[Called <name> with args:
/// {...}]` text convention, which some Kiro model outputs embed directly in
/// otherwise-plain content instead of using structured tool-call events.
///
/// For each `[Called <name> with args:` match, the argument object is
/// located by scanning forward for the next `{` and using
/// [`find_matching_brace`] to find its closing `}` (so nested braces and
/// braces inside string values inside the arguments are handled correctly).
/// Matches whose argument JSON fails to parse are skipped (logged, not
/// fatal) rather than aborting the whole extraction. Returns an empty list
/// immediately if the text doesn't contain the literal substring `"[Called"`,
/// as a fast path to avoid running the regex on ordinary content.
///
/// # Examples
///
/// ```
/// use lanius_core::upstream::parse_bracket_tool_calls;
///
/// let text = r#"[Called read_file with args: {"path": "x.txt"}]"#;
/// let calls = parse_bracket_tool_calls(text);
/// assert_eq!(calls[0].name, "read_file");
/// ```
pub fn parse_bracket_tool_calls(response_text: &str) -> Vec<ToolCall> {
    use once_cell::sync::Lazy;
    use regex::Regex;

    static PATTERN: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\[Called\s+(\w+)\s+with\s+args:\s*")
            .unwrap_or_else(|_| unreachable!("static bracket-call regex must be valid"))
    });

    if response_text.is_empty() || !response_text.contains("[Called") {
        return Vec::new();
    }

    let mut out = Vec::new();
    for caps in PATTERN.captures_iter(response_text) {
        let (Some(whole), Some(func_name)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let func_name = func_name.as_str();

        let Some(rel) = response_text[whole.end()..].find('{') else {
            continue;
        };
        let json_start = whole.end() + rel;
        let Some(json_end) = find_matching_brace(response_text, json_start) else {
            continue;
        };

        let json_str = &response_text[json_start..=json_end];
        match serde_json::from_str::<Value>(json_str) {
            Ok(args) => out.push(ToolCall {
                id: Some(generate_tool_call_id()),
                name: func_name.to_string(),
                arguments: format_json_spaced(&args),
                truncation: None,
            }),
            Err(_) => {
                tracing::warn!(
                    fragment = %char_prefix(json_str, 100),
                    "failed to parse bracket tool call arguments"
                );
            }
        }
    }
    out
}

/// A structured event decoded from Kiro's response stream by
/// [`AwsEventStreamParser`].
#[derive(Debug, Clone, PartialEq)]
pub enum ParserEvent {
    /// A chunk of assistant text content.
    Content(Value),
    /// A native reasoning (`reasoningContentEvent`) fragment: summarized
    /// thinking text and/or the opaque signature Kiro sends once the thinking
    /// block is complete.
    Reasoning {
        /// Summarized thinking text fragment, when present in this event.
        text: Option<String>,
        /// Opaque signature Kiro sends once the thinking block is complete.
        signature: Option<String>,
    },
    /// Usage/billing information reported by Kiro.
    Usage(Value),
    /// Percentage of the model's context window consumed so far.
    ContextUsage(Value),
}

// Recognized JSON-fragment key prefixes and the event kind each one starts.
// The parser scans the buffer for the *earliest* occurrence of any of these
// literal prefixes to decide which fragment to parse next; tool-call
// lifecycle events (ToolStart/ToolInput/ToolStop) don't map to a
// `ParserEvent` directly — they update internal tool-call accumulation
// state instead (see `process_event`).
const EVENT_PATTERNS: &[(&str, PatternKind)] = &[
    ("{\"content\":", PatternKind::Content),
    // `reasoningContentEvent` payloads: `{"text":...}` chunks, then a final
    // `{"signature":...}` (some models send both keys in one event).
    ("{\"text\":", PatternKind::Reasoning),
    ("{\"signature\":", PatternKind::Reasoning),
    ("{\"name\":", PatternKind::ToolStart),
    ("{\"input\":", PatternKind::ToolInput),
    ("{\"stop\":", PatternKind::ToolStop),
    ("{\"followupPrompt\":", PatternKind::Followup),
    ("{\"usage\":", PatternKind::Usage),
    // `meteringEvent` payloads: `{"unit":"credit","unitPlural":"credits","usage":0.03}`.
    ("{\"unit\":", PatternKind::Metering),
    ("{\"contextUsagePercentage\":", PatternKind::ContextUsage),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternKind {
    Content,
    Reasoning,
    ToolStart,
    ToolInput,
    ToolStop,
    Followup,
    Usage,
    Metering,
    ContextUsage,
}

/// Incremental decoder for Kiro's response stream. Create one instance per
/// response and feed it raw bytes as they arrive via [`feed`](Self::feed);
/// it internally buffers partial UTF-8 sequences and partial JSON objects
/// across calls, so chunk boundaries never need to align with character or
/// object boundaries. Not safe to share across concurrent streams.
#[derive(Debug, Default)]
pub struct AwsEventStreamParser {
    pending_bytes: Vec<u8>,
    buffer: String,
    last_content: Option<Value>,
    current_tool_call: Option<PartialToolCall>,
    tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone)]
struct PartialToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl AwsEventStreamParser {
    /// Creates a new parser with empty internal state.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::AwsEventStreamParser;
    ///
    /// let parser = AwsEventStreamParser::new();
    /// assert!(!parser.has_tool_calls());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the next chunk of raw response bytes, returning any
    /// [`ParserEvent`]s that could be fully decoded from the buffer so far.
    /// Tool-call events are never returned here — they are only exposed via
    /// [`take_tool_calls`](Self::take_tool_calls) once finalized.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::{AwsEventStreamParser, ParserEvent};
    ///
    /// let mut parser = AwsEventStreamParser::new();
    /// let events = parser.feed(br#"{"content":"hello"}"#);
    /// assert!(matches!(events[0], ParserEvent::Content(_)));
    /// ```
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<ParserEvent> {
        self.decode_into_buffer(chunk);
        self.drain_buffer()
    }

    // Appends `chunk` to the byte buffer and decodes as much valid UTF-8 as
    // possible into the text buffer, retaining any trailing incomplete
    // multi-byte sequence in `pending_bytes` for the next call. On invalid
    // UTF-8 (as opposed to merely incomplete), the malformed bytes are
    // dropped so a single corrupt sequence can't wedge the parser forever;
    // this loop re-runs after each invalid sequence to keep decoding
    // whatever valid UTF-8 remains afterward.
    fn decode_into_buffer(&mut self, chunk: &[u8]) {
        self.pending_bytes.extend_from_slice(chunk);

        loop {
            match std::str::from_utf8(&self.pending_bytes) {
                Ok(s) => {
                    self.buffer.push_str(s);
                    self.pending_bytes.clear();
                    return;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    if valid_up_to > 0 {
                        if let Ok(valid) = std::str::from_utf8(&self.pending_bytes[..valid_up_to]) {
                            self.buffer.push_str(valid);
                        }
                    }
                    match e.error_len() {
                        // `None` means the invalid tail is merely incomplete
                        // (more bytes may complete it) — keep it buffered
                        // for the next `feed` call.
                        None => {
                            self.pending_bytes.drain(..valid_up_to);
                            return;
                        }
                        // `Some(bad)` means those bytes are genuinely
                        // invalid, not just incomplete — drop them and keep
                        // trying to decode whatever follows.
                        Some(bad) => {
                            self.pending_bytes.drain(..valid_up_to + bad);
                            if self.pending_bytes.is_empty() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    // Repeatedly finds the earliest-occurring recognized JSON-fragment
    // prefix in the text buffer, locates its matching closing brace, parses
    // it, and processes it — stopping as soon as no complete fragment can
    // be found (leaving any trailing partial fragment buffered for the next
    // `feed` call). "Earliest" is by string position, not declaration order
    // in `EVENT_PATTERNS`, since fragments can arrive in any order.
    fn drain_buffer(&mut self) -> Vec<ParserEvent> {
        let mut events = Vec::new();

        loop {
            let mut earliest: Option<(usize, PatternKind)> = None;
            for (pattern, kind) in EVENT_PATTERNS {
                if let Some(pos) = self.buffer.find(pattern) {
                    match earliest {
                        Some((best, _)) if pos >= best => {}
                        _ => earliest = Some((pos, *kind)),
                    }
                }
            }

            let Some((pos, kind)) = earliest else { break };

            let Some(end) = find_matching_brace(&self.buffer, pos) else {
                break;
            };

            let json_str = self.buffer[pos..=end].to_string();
            self.buffer.drain(..=end);

            match serde_json::from_str::<Value>(&json_str) {
                Ok(data) => {
                    if let Some(event) = self.process_event(&data, kind) {
                        events.push(event);
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        fragment = %char_prefix(&json_str, 100),
                        "failed to parse JSON event"
                    );
                }
            }
        }

        events
    }

    fn process_event(&mut self, data: &Value, kind: PatternKind) -> Option<ParserEvent> {
        match kind {
            PatternKind::Content => self.process_content(data),
            PatternKind::Reasoning => Self::process_reasoning(data),
            PatternKind::ToolStart => {
                self.process_tool_start(data);
                None
            }
            PatternKind::ToolInput => {
                self.process_tool_input(data);
                None
            }
            PatternKind::ToolStop => {
                self.process_tool_stop(data);
                None
            }
            PatternKind::Usage => Some(ParserEvent::Usage(
                data.get("usage").cloned().unwrap_or(Value::from(0)),
            )),
            // The whole metering object is kept so downstream formatters can
            // read the credit amount as well as any cache token counts.
            PatternKind::Metering => Some(ParserEvent::Usage(data.clone())),
            PatternKind::ContextUsage => Some(ParserEvent::ContextUsage(
                data.get("contextUsagePercentage")
                    .cloned()
                    .unwrap_or(Value::from(0)),
            )),
            PatternKind::Followup => None,
        }
    }

    // Extracts a content event, applying two Kiro-specific quirks: content
    // events carrying a truthy `followupPrompt` are suggestion metadata, not
    // real output, and are dropped; and Kiro sometimes repeats the exact
    // same content value back-to-back, which is deduplicated against
    // `last_content` (a single leading `null` is also swallowed as a
    // stream-start artifact, but only the first time — a later `null` after
    // real content would be treated as new, distinct content in principle,
    // though in practice Kiro doesn't seem to do that).
    fn process_content(&mut self, data: &Value) -> Option<ParserEvent> {
        if is_truthy(data.get("followupPrompt")) {
            return None;
        }

        let content = data.get("content").cloned().unwrap_or(Value::from(""));

        if self.last_content.as_ref() == Some(&content) {
            return None;
        }
        if content.is_null() && self.last_content.is_none() {
            self.last_content = Some(content);
            return None;
        }

        self.last_content = Some(content.clone());
        Some(ParserEvent::Content(content))
    }

    // Extracts a native reasoning fragment. Unlike content, reasoning chunks are
    // not deduplicated: short fragments (e.g. " the") legitimately repeat.
    // When the reasoning is hidden (GPT models, or Claude with `display:
    // omitted`), Kiro sends a single `{"signature":..,"text":"..."}` event;
    // that `"..."` is a redaction placeholder, not thinking text, so it is
    // dropped. Events carrying neither a non-empty text nor a signature are
    // dropped entirely.
    fn process_reasoning(data: &Value) -> Option<ParserEvent> {
        let field = |key: &str| {
            data.get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let signature = field("signature");
        let text = field("text").filter(|text| signature.is_none() || text != "...");
        (text.is_some() || signature.is_some())
            .then_some(ParserEvent::Reasoning { text, signature })
    }

    // Begins accumulating a new tool call. If one was already in progress
    // (Kiro started a new tool call before stopping the previous one),
    // that previous call is finalized first rather than discarded. The id
    // is generated locally only when the `toolUseId` key is entirely
    // absent — an explicit `null` or empty string is preserved as-is, since
    // those are meaningfully different from "Kiro didn't send an id at
    // all". If `stop` is already true on this same event (a call that
    // starts and ends in one shot, with no separate input/stop events),
    // it's finalized immediately.
    fn process_tool_start(&mut self, data: &Value) {
        if self.current_tool_call.is_some() {
            self.finalize_tool_call();
        }

        self.current_tool_call = Some(PartialToolCall {
            id: match data.get("toolUseId") {
                None => Some(generate_tool_call_id()),
                Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(other) => Some(other.to_string()),
            },
            name: data
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            arguments: stringify_input(data.get("input")),
        });

        if is_truthy(data.get("stop")) {
            self.finalize_tool_call();
        }
    }

    // Appends another fragment of argument text to the in-progress tool
    // call, if any. Silently ignored if there's no open tool call, since an
    // input fragment arriving without a preceding start event has nothing
    // to attach to.
    fn process_tool_input(&mut self, data: &Value) {
        if let Some(tc) = self.current_tool_call.as_mut() {
            tc.arguments.push_str(&stringify_input(data.get("input")));
        }
    }

    fn process_tool_stop(&mut self, data: &Value) {
        if self.current_tool_call.is_some() && is_truthy(data.get("stop")) {
            self.finalize_tool_call();
        }
    }

    // Moves the in-progress tool call (if any) into `tool_calls`, parsing
    // its accumulated raw argument text as JSON. An empty/whitespace-only
    // argument string becomes `"{}"` (a call with no arguments). If parsing
    // fails, `diagnose_json_truncation` is consulted: a truncation is
    // logged at error level and recorded on the call (so downstream
    // recovery-prompt logic can react to it); any other parse failure is
    // just a warning, since it likely reflects a genuinely malformed
    // response rather than a size-limit cutoff. Either way, the arguments
    // fall back to `"{}"` so a bad payload never propagates as invalid
    // JSON to the client.
    fn finalize_tool_call(&mut self) {
        let Some(partial) = self.current_tool_call.take() else {
            return;
        };

        let raw = partial.arguments;
        let mut truncation = None;

        let arguments = if raw.trim().is_empty() {
            "{}".to_string()
        } else {
            match serde_json::from_str::<Value>(&raw) {
                Ok(parsed) => format_json_spaced(&parsed),
                Err(e) => {
                    let info = diagnose_json_truncation(&raw);
                    if info.is_truncated {
                        tracing::error!(
                            tool = %partial.name,
                            id = ?partial.id,
                            size_bytes = info.size_bytes,
                            reason = %info.reason,
                            "tool call truncated by upstream Kiro API"
                        );
                        truncation = Some(info);
                    } else {
                        tracing::warn!(
                            tool = %partial.name,
                            error = %e,
                            raw = %char_prefix(&raw, 200),
                            "failed to parse tool arguments"
                        );
                    }
                    "{}".to_string()
                }
            }
        };

        self.tool_calls.push(ToolCall {
            id: partial.id,
            name: partial.name,
            arguments,
            truncation,
        });
    }

    /// Finalizes any still-in-progress tool call, then returns all
    /// collected tool calls run through [`deduplicate_tool_calls`],
    /// draining them from the parser. Safe to call once the stream has
    /// ended (or at any point you want a snapshot; subsequent calls only
    /// return calls collected since the last drain).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::AwsEventStreamParser;
    ///
    /// let mut parser = AwsEventStreamParser::new();
    /// parser.feed(br#"{"name":"read","toolUseId":"1"}"#);
    /// parser.feed(br#"{"input":"{}","stop":true}"#);
    /// let calls = parser.take_tool_calls();
    /// assert_eq!(calls[0].name, "read");
    /// ```
    pub fn take_tool_calls(&mut self) -> Vec<ToolCall> {
        if self.current_tool_call.is_some() {
            self.finalize_tool_call();
        }
        deduplicate_tool_calls(std::mem::take(&mut self.tool_calls))
    }

    /// Whether any tool call has been finalized or is currently in
    /// progress.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::AwsEventStreamParser;
    ///
    /// let mut parser = AwsEventStreamParser::new();
    /// assert!(!parser.has_tool_calls());
    /// parser.feed(br#"{"name":"read","toolUseId":"1"}"#);
    /// assert!(parser.has_tool_calls());
    /// ```
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty() || self.current_tool_call.is_some()
    }

    /// Resets the parser to a fresh, empty state, discarding all buffered
    /// bytes and any accumulated tool calls, so the instance can be reused
    /// for a new stream.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::upstream::AwsEventStreamParser;
    ///
    /// let mut parser = AwsEventStreamParser::new();
    /// parser.feed(br#"{"name":"read","toolUseId":"1"}"#);
    /// parser.reset();
    /// assert!(!parser.has_tool_calls());
    /// ```
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// Returns the first `max_chars` Unicode scalar values of `text` (or the
// whole string if shorter), used to keep log fragments bounded without
// risking a panic from slicing mid-character.
fn char_prefix(text: &str, max_chars: usize) -> &str {
    text.char_indices()
        .nth(max_chars)
        .map_or(text, |(index, _)| &text[..index])
}

// Truthiness for a JSON value, following the same absent/null/false/0/""/
// []/{} == falsy convention Kiro's own JSON-based protocol uses: everything
// else (including non-zero numbers and non-empty strings/collections) is
// truthy. Used to interpret Kiro's `stop`/`followupPrompt` fields.
fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

// Renders a tool-call `input` fragment as the text to append to the
// accumulating arguments buffer: a non-empty object is re-serialized with
// spaced JSON (so partial-object fragments and full-object fragments are
// represented consistently); a string fragment is used verbatim (this
// is the common case — Kiro streams `input` as raw JSON text split across
// multiple events); a non-empty array is likewise JSON-rendered; empty
// containers and other falsy values contribute nothing.
fn stringify_input(input: Option<&Value>) -> String {
    match input {
        Some(Value::Object(map)) => {
            if map.is_empty() {
                String::new()
            } else {
                format_json_spaced(&Value::Object(map.clone()))
            }
        }
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) if !a.is_empty() => format_json_spaced(&Value::Array(a.clone())),
        Some(v) if is_truthy(Some(v)) => v.to_string(),
        _ => String::new(),
    }
}

/// Diagnoses why a string that failed to parse as JSON is malformed,
/// specifically trying to distinguish "cut off mid-stream by Kiro's output
/// size limit" from "malformed for some other reason":
///
/// - Empty/whitespace-only input is reported as not truncated (there's
///   nothing to have been cut off).
/// - If the string starts with `{`/`[` but doesn't end with the matching
///   `}`/`]`, it's almost certainly a truncated object/array; the reported
///   "missing N closing brace/bracket(s)" count comes from the raw
///   open/close character counts (a rough signal, not a precise nesting
///   analysis).
/// - Otherwise, an overall open/close count mismatch for braces or brackets
///   is still reported as truncated, even if the string happens to
///   start/end with the right characters.
/// - An odd number of unescaped `"` characters indicates an unclosed string
///   literal, also treated as truncation.
/// - Anything else is reported as truncated = false, reason "malformed
///   JSON" — balanced brackets/braces and quotes but still invalid JSON
///   suggests a different kind of corruption, not a size-limit cutoff.
///
/// # Examples
///
/// ```
/// use lanius_core::upstream::diagnose_json_truncation;
///
/// let info = diagnose_json_truncation(r#"{"city": "Toky"#);
/// assert!(info.is_truncated);
/// ```
pub fn diagnose_json_truncation(json_str: &str) -> TruncationInfo {
    let size_bytes = json_str.len();
    let stripped = json_str.trim();

    if stripped.is_empty() {
        return TruncationInfo {
            is_truncated: false,
            reason: "empty string".into(),
            size_bytes,
        };
    }

    let open_braces = stripped.matches('{').count();
    let close_braces = stripped.matches('}').count();
    let open_brackets = stripped.matches('[').count();
    let close_brackets = stripped.matches(']').count();

    if stripped.starts_with('{') && !stripped.ends_with('}') {
        let missing = open_braces as i64 - close_braces as i64;
        return TruncationInfo {
            is_truncated: true,
            reason: format!("missing {missing} closing brace(s)"),
            size_bytes,
        };
    }

    if stripped.starts_with('[') && !stripped.ends_with(']') {
        let missing = open_brackets as i64 - close_brackets as i64;
        return TruncationInfo {
            is_truncated: true,
            reason: format!("missing {missing} closing bracket(s)"),
            size_bytes,
        };
    }

    if open_braces != close_braces {
        return TruncationInfo {
            is_truncated: true,
            reason: format!("unbalanced braces ({open_braces} open, {close_braces} close)"),
            size_bytes,
        };
    }

    if open_brackets != close_brackets {
        return TruncationInfo {
            is_truncated: true,
            reason: format!("unbalanced brackets ({open_brackets} open, {close_brackets} close)"),
            size_bytes,
        };
    }

    // Count unescaped double-quotes, skipping any `\X` escape pair so an
    // escaped backslash-quote sequence doesn't miscount as ending a string.
    let bytes = stripped.as_bytes();
    let mut quote_count = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            quote_count += 1;
        }
        i += 1;
    }
    if quote_count % 2 != 0 {
        return TruncationInfo {
            is_truncated: true,
            reason: "unclosed string literal".into(),
            size_bytes,
        };
    }

    TruncationInfo {
        is_truncated: false,
        reason: "malformed JSON".into(),
        size_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contents(events: &[ParserEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::Content(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn finds_simple_closing_brace() {
        assert_eq!(find_matching_brace(r#"{"a": 1}"#, 0), Some(7));
    }

    #[test]
    fn handles_nested_objects() {
        assert_eq!(find_matching_brace(r#"{"a": {"b": 1}}"#, 0), Some(14));
    }

    #[test]
    fn ignores_braces_inside_strings() {
        assert_eq!(find_matching_brace(r#"{"a": "{}"}"#, 0), Some(10));
    }

    #[test]
    fn ignores_escaped_quotes() {
        let text = r#"{"a": "he said \"hi\" {"}"#;
        let end = find_matching_brace(text, 0).expect("should find closing brace");
        assert_eq!(&text[..=end], text);
    }

    #[test]
    fn returns_none_for_incomplete_object() {
        assert_eq!(find_matching_brace(r#"{"a": 1"#, 0), None);
    }

    #[test]
    fn returns_none_when_not_starting_at_brace() {
        assert_eq!(find_matching_brace(r#"x{"a":1}"#, 0), None);
        assert_eq!(find_matching_brace("", 0), None);
        assert_eq!(find_matching_brace("{}", 99), None);
    }

    #[test]
    fn brace_matching_is_utf8_safe() {
        let text = r#"{"t": "中文测试"}"#;
        let end = find_matching_brace(text, 0).unwrap();
        assert_eq!(&text[..=end], text);
    }

    #[test]
    fn invalid_multibyte_json_at_log_boundary_does_not_panic() {
        let payload = format!(r#"{{"content":"{}中",}}"#, "a".repeat(87));
        assert!(!payload.is_char_boundary(100));
        assert_eq!(char_prefix(&payload, 100).chars().count(), 100);

        let result = std::panic::catch_unwind(|| {
            let mut parser = AwsEventStreamParser::new();
            parser.feed(payload.as_bytes())
        });
        assert!(
            result.is_ok(),
            "invalid UTF-8-adjacent JSON logging must not panic"
        );
    }

    #[test]
    fn parses_single_content_event() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"content":"Hello"}"#);
        assert_eq!(contents(&events), vec!["Hello"]);
    }

    #[test]
    fn deduplicates_repeated_content() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"content":"A"}{"content":"A"}"#);
        assert_eq!(contents(&events), vec!["A"]);
    }

    #[test]
    fn dedup_is_not_merely_adjacent() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"content":"A"}{"usage":5}{"content":"A"}"#);
        assert_eq!(contents(&events), vec!["A"]);
        assert!(events.iter().any(|e| matches!(e, ParserEvent::Usage(_))));
    }

    #[test]
    fn different_content_is_not_deduplicated() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"content":"A"}{"content":"B"}{"content":"A"}"#);
        assert_eq!(contents(&events), vec!["A", "B", "A"]);
    }

    #[test]
    fn leading_null_content_is_dropped() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"content":null}"#);
        assert!(events.is_empty(), "got {events:?}");
    }

    #[test]
    fn content_with_followup_prompt_is_skipped() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"content":"x","followupPrompt":{"a":1}}"#);
        assert!(events.is_empty());
    }

    #[test]
    fn object_split_across_chunks_is_reassembled() {
        let mut p = AwsEventStreamParser::new();
        assert!(p.feed(br#"{"content":"Hel"#).is_empty());
        let events = p.feed(br#"lo World"}"#);
        assert_eq!(contents(&events), vec!["Hello World"]);
    }

    #[test]
    fn multibyte_char_split_across_chunks_survives() {
        let full = r#"{"content":"中文"}"#.as_bytes().to_vec();
        let split = 13;
        let mut p = AwsEventStreamParser::new();
        assert!(p.feed(&full[..split]).is_empty());
        let events = p.feed(&full[split..]);
        assert_eq!(contents(&events), vec!["中文"]);
    }

    #[test]
    fn byte_at_a_time_feeding_matches_whole_feeding() {
        let payload = br#"{"content":"alpha"}{"content":"beta"}{"usage":3}"#;

        let mut whole = AwsEventStreamParser::new();
        let expected = whole.feed(payload);

        let mut drip = AwsEventStreamParser::new();
        let mut got = Vec::new();
        for b in payload {
            got.extend(drip.feed(&[*b]));
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn parses_reasoning_text_and_signature_without_dedup() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(
            br#"{"text":"a"}{"text":"a"}{"content":"answer"}{"signature":"sig"}{"signature":"s2","text":"..."}{"text":""}"#,
        );
        assert_eq!(
            events,
            vec![
                ParserEvent::Reasoning {
                    text: Some("a".into()),
                    signature: None
                },
                ParserEvent::Reasoning {
                    text: Some("a".into()),
                    signature: None
                },
                ParserEvent::Content(json!("answer")),
                ParserEvent::Reasoning {
                    text: None,
                    signature: Some("sig".into())
                },
                ParserEvent::Reasoning {
                    text: None,
                    signature: Some("s2".into())
                },
            ]
        );
        let events = p.feed(br#"{"text":"..."}"#);
        assert_eq!(
            events,
            vec![ParserEvent::Reasoning {
                text: Some("...".into()),
                signature: None
            }],
            "a streamed \"...\" chunk without a signature is real text"
        );
    }

    #[test]
    fn escaped_text_key_inside_content_is_not_reasoning() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"content":"see {\"text\": 1}"}"#);
        assert_eq!(contents(&events), vec![r#"see {"text": 1}"#]);
    }

    #[test]
    fn parses_usage_and_context_usage() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"usage":7}{"contextUsagePercentage":42.5}"#);
        assert_eq!(
            events,
            vec![
                ParserEvent::Usage(json!(7)),
                ParserEvent::ContextUsage(json!(42.5)),
            ]
        );
    }

    #[test]
    fn explicit_null_usage_propagates_as_null() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"usage":null}"#);
        assert_eq!(events, vec![ParserEvent::Usage(Value::Null)]);
    }

    #[test]
    fn usage_can_be_an_object() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"usage":{"credits":1.3}}"#);
        assert_eq!(events, vec![ParserEvent::Usage(json!({"credits": 1.3}))]);
    }

    #[test]
    fn metering_event_is_parsed_as_usage() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(
            br#"{"contextUsagePercentage":1.9}{"unit":"credit","unitPlural":"credits","usage":0.032}"#,
        );
        assert_eq!(
            events,
            vec![
                ParserEvent::ContextUsage(json!(1.9)),
                ParserEvent::Usage(json!({"unit":"credit","unitPlural":"credits","usage":0.032})),
            ]
        );
    }

    #[test]
    fn structured_tool_call_emitted_only_at_end() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"name":"get_weather","toolUseId":"call_1","input":{}}"#);
        assert!(events.is_empty(), "tool events must not stream");

        let events = p.feed(br#"{"input":"{\"city\":\"London\"}"}{"stop":true}"#);
        assert!(events.is_empty());

        let calls = p.take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(calls[0].arguments, r#"{"city": "London"}"#);
    }

    #[test]
    fn tool_arguments_use_spaced_non_compact_json() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"f","toolUseId":"c1","input":"{\"a\":1,\"b\":2}"}{"stop":true}"#);
        let calls = p.take_tool_calls();
        assert_eq!(calls[0].arguments, r#"{"a": 1, "b": 2}"#);
    }

    #[test]
    fn tool_use_id_generated_only_when_key_absent() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"f","input":{"a":1},"stop":true}"#);
        let calls = p.take_tool_calls();
        assert!(calls[0].id.as_deref().unwrap().starts_with("call_"));

        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"g","toolUseId":null,"input":{"a":1},"stop":true}"#);
        let calls = p.take_tool_calls();
        assert_eq!(calls[0].id, None, "explicit null must not be replaced");

        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"h","toolUseId":"","input":{"a":1},"stop":true}"#);
        let calls = p.take_tool_calls();
        assert_eq!(calls[0].id.as_deref(), Some(""), "empty string preserved");
    }

    #[test]
    fn inline_stop_finalizes_immediately() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"f","toolUseId":"c1","input":{"x":1},"stop":true}"#);
        assert_eq!(p.take_tool_calls().len(), 1);
    }

    #[test]
    fn stop_false_does_not_finalize() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"f","toolUseId":"c1","input":{"x":1}}"#);
        p.feed(br#"{"stop":false}"#);
        assert!(p.has_tool_calls());
        assert_eq!(p.take_tool_calls().len(), 1);
    }

    #[test]
    fn new_tool_start_finalizes_previous() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"a","toolUseId":"c1","input":{"x":1}}"#);
        p.feed(br#"{"name":"b","toolUseId":"c2","input":{"y":2}}"#);
        let calls = p.take_tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].name, "b");
    }

    #[test]
    fn empty_object_input_yields_empty_string_then_fragments() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"f","toolUseId":"c1","input":{}}"#);
        p.feed(br#"{"input":"{\"k\":"}"#);
        p.feed(br#"{"input":"1}"}"#);
        p.feed(br#"{"stop":true}"#);
        let calls = p.take_tool_calls();
        assert_eq!(calls[0].arguments, r#"{"k": 1}"#);
    }

    #[test]
    fn input_fragment_without_open_tool_is_ignored() {
        let mut p = AwsEventStreamParser::new();
        let events = p.feed(br#"{"input":"{\"a\":1}"}"#);
        assert!(events.is_empty());
        assert!(!p.has_tool_calls());
    }

    #[test]
    fn unparseable_arguments_become_empty_object() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"f","toolUseId":"c1","input":"not json at all"}{"stop":true}"#);
        let calls = p.take_tool_calls();
        assert_eq!(calls[0].arguments, "{}");
    }

    #[test]
    fn diagnoses_missing_closing_brace_with_exact_size() {
        let args = r#"{"filePath": "/Users/cc/Documents/Code/mock-all/docs/plans/2026-01-12-mock-all-impl.md""#;
        let info = diagnose_json_truncation(args);
        assert!(info.is_truncated);
        assert!(info.reason.contains("closing brace"), "got {}", info.reason);
        assert_eq!(info.size_bytes, 87);
    }

    #[test]
    fn diagnoses_unclosed_string() {
        let info = diagnose_json_truncation(r#"{"a": "unterminated}"#);
        assert!(info.is_truncated);
    }

    #[test]
    fn diagnoses_malformed_but_not_truncated() {
        let info = diagnose_json_truncation("{not json}");
        assert!(!info.is_truncated);
        assert_eq!(info.reason, "malformed JSON");
    }

    #[test]
    fn diagnoses_empty_string() {
        let info = diagnose_json_truncation("   ");
        assert!(!info.is_truncated);
        assert_eq!(info.reason, "empty string");
    }

    #[test]
    fn truncated_tool_call_is_flagged() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"name":"write","toolUseId":"c1","input":"{\"path\":\"/a/b"}"#);
        p.feed(br#"{"stop":true}"#);
        let calls = p.take_tool_calls();
        assert_eq!(calls[0].arguments, "{}");
        let t = calls[0]
            .truncation
            .as_ref()
            .expect("should flag truncation");
        assert!(t.is_truncated);
    }

    #[test]
    fn dedup_prefers_richer_arguments_for_same_id() {
        let calls = vec![
            ToolCall {
                id: Some("call_abc".into()),
                name: "search".into(),
                arguments: r#"{"q": "test"}"#.into(),
                truncation: None,
            },
            ToolCall {
                id: Some("call_abc".into()),
                name: "search".into(),
                arguments: r#"{"q": "test", "limit": 10, "offset": 0}"#.into(),
                truncation: None,
            },
        ];
        let unique = deduplicate_tool_calls(calls);
        assert_eq!(unique.len(), 1);
        assert!(unique[0].arguments.contains("limit"));
    }

    #[test]
    fn dedup_replaces_empty_object_arguments() {
        let calls = vec![
            ToolCall {
                id: Some("c1".into()),
                name: "f".into(),
                arguments: "{}".into(),
                truncation: None,
            },
            ToolCall {
                id: Some("c1".into()),
                name: "f".into(),
                arguments: r#"{"a": 1}"#.into(),
                truncation: None,
            },
        ];
        let unique = deduplicate_tool_calls(calls);
        assert_eq!(unique[0].arguments, r#"{"a": 1}"#);
    }

    #[test]
    fn dedup_collapses_identical_name_and_args_across_ids() {
        let calls = vec![
            ToolCall {
                id: Some("c1".into()),
                name: "f".into(),
                arguments: r#"{"a": 1}"#.into(),
                truncation: None,
            },
            ToolCall {
                id: Some("c2".into()),
                name: "f".into(),
                arguments: r#"{"a": 1}"#.into(),
                truncation: None,
            },
        ];
        assert_eq!(deduplicate_tool_calls(calls).len(), 1);
    }

    #[test]
    fn dedup_keeps_distinct_calls() {
        let calls = vec![
            ToolCall {
                id: Some("c1".into()),
                name: "f".into(),
                arguments: r#"{"a": 1}"#.into(),
                truncation: None,
            },
            ToolCall {
                id: Some("c2".into()),
                name: "g".into(),
                arguments: r#"{"a": 1}"#.into(),
                truncation: None,
            },
        ];
        assert_eq!(deduplicate_tool_calls(calls).len(), 2);
    }

    #[test]
    fn dedup_places_id_calls_before_idless_ones() {
        let calls = vec![
            ToolCall {
                id: None,
                name: "noid".into(),
                arguments: "{}".into(),
                truncation: None,
            },
            ToolCall {
                id: Some("c1".into()),
                name: "withid".into(),
                arguments: "{}".into(),
                truncation: None,
            },
        ];
        let unique = deduplicate_tool_calls(calls);
        assert_eq!(unique[0].name, "withid");
        assert_eq!(unique[1].name, "noid");
    }

    #[test]
    fn parses_bracket_tool_call() {
        let calls =
            parse_bracket_tool_calls(r#"[Called get_weather with args: {"city": "London"}]"#);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, r#"{"city": "London"}"#);
        assert!(calls[0].id.as_deref().unwrap().starts_with("call_"));
    }

    #[test]
    fn parses_bracket_tool_call_with_nested_json() {
        let text = r#"[Called complex_func with args: {"data": {"nested": {"deep": "value"}}}]"#;
        let calls = parse_bracket_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "complex_func");
        assert!(calls[0].arguments.contains("deep"));
    }

    #[test]
    fn bracket_parsing_needs_no_trailing_bracket() {
        let calls = parse_bracket_tool_calls(r#"[Called f with args: {"a": 1}"#);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn bracket_parsing_guard_is_case_sensitive_despite_insensitive_regex() {
        assert!(parse_bracket_tool_calls(r#"[called f with args: {"a": 1}"#).is_empty());
        assert_eq!(
            parse_bracket_tool_calls(r#"[Called f WITH ARGS: {"a": 1}"#).len(),
            1
        );
    }

    #[test]
    fn bracket_parsing_returns_empty_without_marker() {
        assert!(parse_bracket_tool_calls("just some prose").is_empty());
        assert!(parse_bracket_tool_calls("").is_empty());
    }

    #[test]
    fn bracket_parsing_skips_unparseable_args() {
        assert!(parse_bracket_tool_calls(r#"[Called f with args: {broken"#).is_empty());
    }

    #[test]
    fn reset_clears_all_state() {
        let mut p = AwsEventStreamParser::new();
        p.feed(br#"{"content":"A"}{"name":"f","toolUseId":"c1","input":{"a":1}}"#);
        p.reset();
        assert!(!p.has_tool_calls());
        let events = p.feed(br#"{"content":"A"}"#);
        assert_eq!(contents(&events), vec!["A"]);
    }
}
