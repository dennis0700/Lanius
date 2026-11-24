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

## Kiro hosts and client identity

Lanius talks to Kiro the way the default (V3) agent of Kiro CLI 3.5.0 does:
the same two service hosts, both on Kiro's
[firewall allowlist](https://kiro.dev/docs/privacy-and-security/firewalls/),
and the same request shape, captured from live `kiro-cli` 3.5.0 traffic.

- **Runtime host** (`KIRO_API_HOST_TEMPLATE`, `runtime.<region>.kiro.dev`):
  `KiroRuntimeService.GenerateAssistantResponse` and
  `KiroRuntimeService.InvokeMCP`.
- **Management host** (`KIRO_MANAGEMENT_HOST_TEMPLATE`,
  `management.<region>.kiro.dev`): `KiroControlPlaneBearerService`'s
  `ListAvailableProfiles`, `ListAvailableModels`, `GetUsageLimits` and
  `GetProfile`. Inputs go in the JSON body only (Kiro CLI 2.x also repeated
  them in the query string).

Every request identifies as the interactive Kiro CLI's agent harness (KAS
0.66.119, on the AWS SDK for JavaScript), for example:

```
User-Agent: aws-sdk-js/1.0.0 ua/2.1 os/linux#<kernel release> lang/js md/nodejs#22.23.4
  api/kiroruntime#1.0.0 m/N KiroCLI/3.5.0 KAS/0.66.119 os/linux md/appVersion-3.5.0
  app/AmazonQ-For-CLI acp-client/kiro-cli
x-amz-user-agent: aws-sdk-js/1.0.0 KiroCLI/3.5.0 KAS/0.66.119 os/linux
  md/appVersion-3.5.0 app/AmazonQ-For-CLI acp-client/kiro-cli
```

The `os/` values are Node.js names (`darwin`, `linux`, `win32`). Chat
requests add `x-amzn-kiro-client-attribution: kiro-cli`,
`x-amzn-kiro-mode-identifier: default-v2`, `x-amzn-kiro-fallback: 0;max=0`
and `x-amzn-kiro-attempt: 1;max=3`. `TokenType` is `API_KEY` for API keys
and `SSO_OIDC` for IAM Identity Center logins; social logins send none.
Lanius keeps sending `x-amzn-codewhisperer-optout: true`, which the Kiro CLI
sends when the user has not opted in to content collection.

The chat body follows the Kiro CLI too: `conversationState` carries a fresh
`agentContinuationId` per request and the conversation as its own
`rootConversationId`, the request has a top-level `agentMode: "default-v2"`,
no `envState` is sent, and the system prompt opens the history as a user
turn answered by `I will follow these instructions.` (Kiro rejects the
`systemPrompt` field with `REQUEST_BODY_INVALID` unless a server-side flag
allows it).

Kiro marks the older `q.<region>.amazonaws.com` host as legacy (the Kiro CLI
now only sends telemetry there), so Lanius no longer uses it. There is no
client-side endpoint rotation: retries and backoff go to the same host.

## Native reasoning replay

Kiro signs each native reasoning block and checks the signature when the
block is sent back. Lanius replays an assistant turn's reasoning as
`reasoningContent` only when Kiro issued it: an Anthropic `thinking` block
whose signature did not come from Lanius (Lanius's own placeholders start
with `sig_`), or a `redacted_thinking` block, which Lanius produces from
Kiro's `redactedContent`. Kiro answers any other signature with
`THINKING_SIGNATURE_INVALID`, so placeholder-signed blocks are dropped
instead. Server-side `web_search` rounds replay the round's reasoning the
same way.

Kiro also reports why the model stopped (`metadataEvent.stopReason`).
`MAX_TOKENS` marks the response as truncated (`finish_reason: "length"`,
`stop_reason: "max_tokens"`, Responses `incomplete`); without a stop reason
Lanius falls back to its stream-completion heuristic.

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
