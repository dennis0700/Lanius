# Client Compatibility (`compat`)

Kiro (and the AWS control plane behind it) has a handful of quirks that
don't belong in `convert` (shape translation) or `auth` (credentials), but
still need to happen on nearly every request/response. `compat` collects
these as small, independently testable `RequestHook`/`ResponseHook`
implementations run in order by a `CompatibilityPipeline`.

```rust
pub trait RequestHook: Send + Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self) -> bool { true }
    fn apply(&self, request: &mut CompatRequest) -> Result<()>;
}
```

`CompatibilityPipeline::new()` wires up the default hook set:

| Order | Hook | Purpose |
|---|---|---|
| 1 | `ToolNameAliasHook` | placeholder — real logic lives in `ToolNameAliases`, invoked directly from `convert` |
| 2 | `ProfileArnAutofetchHook` | placeholder — real logic invoked directly from `auth::AuthManager` |
| 3 | `ControlPlaneHostHook` | rewrites control-plane-only paths to the AWS host |
| 4 | `ChatHostFallbackHook` | placeholder — real logic invoked directly from `auth::AuthManager::api_host` |
| — | `ModelIdFormatHook` (response) | rewrites `/v1/models` ids for Claude-branded clients |

Request hooks run in registration order; response hooks run in *reverse*
registration order, so the last-registered response hook runs first —
mirroring the request pipeline's outermost-to-innermost wrapping intuition.
Several hooks are no-op placeholders in the pipeline itself (their actual
logic is invoked directly by `convert`/`auth` because those call sites have
information the generic pipeline doesn't) — they still exist here so
`request_hook_names`/`response_hook_names` accurately report which
compatibility behaviors are active, for diagnostics.

## Tool name aliasing

Kiro restricts `toolSpecification.name` to 64 ASCII
alphanumeric/`_`/`-` characters (`MAX_KIRO_TOOL_NAME_LENGTH`), but OpenAI/
Anthropic clients routinely send longer or differently-formatted names —
MCP-style tool names like `mcp__plugin_everything_claude_code_github__create_pull_request_review`
are a common case that exceeds the limit.

`ToolNameAliases::needs_alias` flags any name that's empty, over the length
limit, or contains a byte outside the legal character set. `alias_for`
deterministically maps such a name to a short, collision-avoiding alias
(reusing the same alias for the same name within one request/response
cycle), and `original_for` reverses the mapping.

A single `ToolNameAliases` instance is constructed fresh per request (by
`convert::openai`/`convert::anthropic`, *before* history conversion runs —
see [conversion.md](conversion.md)) so a tool referenced identically in an
old assistant turn and a fresh tool definition receive the same alias.
Restoration happens on the way back out, in three places:

- `original_for` — restores a tool call's `name` field directly.
- `restore_text` — restores any `[Called <alias> with args:...]`-style
  bracket markers embedded in accumulated (non-streaming) text.
- `restore_text_fragment` — the streaming-safe variant: withholds enough
  trailing text on each call to avoid splitting a marker across chunk
  boundaries, only ever emitting text once it's sure no marker is
  straddling the cut point.

See [streaming.md](streaming.md#tool-name-restoration) for where this is
invoked in the response pipeline.

## Host rewriting

Kiro exposes two conceptually different hosts, both currently resolving to
the same `runtime.<region>.kiro.dev` template but kept distinct because
they may diverge and because the *rule* for choosing between them differs
per operation:

- **Control-plane host** (`KIRO_Q_HOST_TEMPLATE`) — model listing, usage
  limits, MCP-related control operations. Always used for these
  operations, unconditionally, via `compat::control_plane_host` /
  `ControlPlaneHostHook`.
- **Chat/runtime host** (`KIRO_API_HOST_TEMPLATE`) — the paid runtime used
  for `generateAssistantResponse`. Requires a profile ARN; accounts without
  one (e.g. Builder ID / free accounts) are routed to the control-plane
  host instead for chat too, since the paid runtime host would reject them.
  This fallback is implemented by `compat::chat_host` and invoked directly
  from `auth::AuthManager::api_host`.

## Model ID formatting

Claude Code and similar Claude-branded clients expect model IDs with dashes
(`claude-sonnet-4-6`) rather than the dotted form Kiro's `/v1/models`
endpoint actually returns (`claude-sonnet-4.6`). `ModelIdFormatHook`
(response hook) and its helpers `rewrite_model_ids`/`is_claude_client`
rewrite `GET /v1/models` responses to the dashed form, but *only* when the
request appears to come from a Claude-branded client — other clients see
Kiro's ids unchanged. This is layered on as `server::model_id_format_middleware`
in `server.rs` so it applies uniformly regardless of which router (OpenAI
or Anthropic-shaped `/v1/models`) served the response.

## `CompatRequest` / `CompatResponse`

The mutable request/response views hooks operate on:

```rust
pub struct CompatRequest {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
    pub upstream_url: Option<String>,  // set by a hook to redirect the request
}

pub struct CompatResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}
```

Both are cheap, self-contained structs so hooks can be unit tested without
constructing a real HTTP request/response.
