# Truncation Detection and Recovery (`truncation`)

Kiro imposes output-size limits that can cut a response off mid-stream: a
tool call's JSON arguments end unbalanced, or plain text stops without a
clean finish. Left unhandled, this either surfaces malformed JSON to the
client or silently returns partial output with no indication anything went
wrong. `truncation` detects this and, on the *next* turn of the same
conversation, injects a notice so the model understands the cutoff was an
API limitation rather than its own error and adapts its approach instead of
repeating the same (likely-truncated-again) call.

## Detecting truncation

Two independent signals, checked at different layers:

- **Tool-call truncation** — diagnosed while parsing (see
  `upstream::parser::diagnose_json_truncation` and
  [streaming.md](streaming.md)): if a tool call's accumulated JSON
  arguments never parse cleanly, the parser explains why (e.g. "missing 2
  closing brace(s)") and replaces `arguments` with `"{}"`, recording the
  diagnosis on `ToolCall::truncation`.
- **Plain-content truncation** — `truncation::is_content_truncated`, a
  heuristic checked after the stream completes: true when the stream did
  *not* finish normally, some content was actually produced, and it wasn't
  a tool call (tool-call truncation is diagnosed separately, as above).

Both response builders (`api::openai::sse::response_value_from_result` and
`api::anthropic::sse::response_from_stream_result`) check these signals
right after collecting the full response and call `TruncationStore::save_*`
to record what happened, keyed by conversation id.

## `TruncationStore`

A thread-safe (`Arc<Mutex<_>>`), TTL-and-capacity-bounded cache
(`DEFAULT_TRUNCATION_TTL` 30 minutes, `DEFAULT_TRUNCATION_CACHE_CAPACITY`
1024 entries, LRU eviction beyond capacity), holding two kinds of record:

- **`ToolTruncationInfo`** — keyed by `(conversation_id, tool_call_id)`;
  carries the tool's name and raw diagnostic JSON.
- **`ContentTruncationInfo`** — keyed by `(conversation_id, message_hash)`
  where the hash is computed over the first 500 characters of the
  truncated content; carries a 200-character preview.

Entries are consumed **exactly once**: every `take_*` method removes the
entry as it returns it, and the `get_*` methods are thin aliases with the
same one-shot semantics (kept only for call-site readability, e.g.
"checking" vs. "consuming" reads intent at the call site even though the
behavior is identical). This means a recovery notice is injected at most
once per truncation event — if the client retries with the same history, it
won't be renotified for a truncation it already saw.

## Recovery injection

On the client's *next* request in the same conversation, the route handlers
(`api::openai::routes::inject_truncation_recovery`,
`api::anthropic::routes::apply_truncation_recovery`) check the store for a
pending record matching the conversation and, if found:

- **Tool-call truncation** → `prepend_tool_recovery_notice` prepends
  `TRUNCATION_TOOL_RESULT_MESSAGE` (a fixed explanatory notice) to the
  original tool result text before it's sent back to the model, so the
  model sees both the explanation and whatever the tool actually returned.
  `generate_truncation_tool_result` builds a synthetic, error-flagged
  `tool_result` block instead, for cases where there is no real tool result
  to prepend to.
- **Plain-content truncation** → `generate_truncation_user_message`
  returns `TRUNCATION_USER_MESSAGE`, a fixed system-notice string informing
  the model its previous response was cut off by the API, not by a mistake
  on its part.

This whole mechanism is gated by `config.truncation_recovery` (default:
enabled) and only ever *adds* explanatory text to an existing turn — it
never fabricates tool results with real-looking data, and it never blocks
or retries the request itself.

## Why this lives outside `convert`/`upstream`

Truncation spans both layers (parser-level diagnosis for tool calls,
stream-completion-level heuristic for text) and persists state *across*
requests (the notice is injected on the request *after* the one that got
truncated) — it doesn't fit cleanly into either `convert` (stateless,
single-request shape translation) or `upstream` (no request-crossing
state). Centralizing it here keeps both of those modules simple and keeps
the recovery-prompt text and cache-eviction policy in one place.
