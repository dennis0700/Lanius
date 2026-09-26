# Architecture

## Workspace layout

Lanius is a Cargo workspace with three crates:

| Crate | Kind | Responsibility |
|---|---|---|
| `lanius-core` | library | The gateway itself — HTTP routes, protocol conversion, auth, streaming parser, model catalog, all business logic. Consumed by both binaries below. |
| `lanius-cli` | binary (`lanius`) | Headless server process, plus `probe`/`replay` debugging subcommands. Thin wrapper: no protocol logic of its own. |
| `lanius-gui` | binary (`lanius-desktop`) | Desktop app (Slint UI) that runs the gateway in-process and adds configuration, live logs, model list, and usage views. |

`lanius-core` is deliberately the only place that understands the OpenAI/
Anthropic/Kiro protocols. Both binaries call into it through a small,
curated public surface (see `crates/lanius-core/src/lib.rs`); everything
else inside `lanius-core` is `pub(crate)` and reached only through each
module's own facade (e.g. `crate::upstream::KiroHttpClient`,
`crate::model::ModelResolver`).

## Why a gateway at all

Kiro (Amazon Q Developer / AWS CodeWhisperer) speaks its own request/
response protocol over a custom AWS event-stream framing, not the OpenAI
Chat Completions API or the Anthropic Messages API. Lanius sits in front of
Kiro and exposes both of those familiar wire formats, so existing tools,
SDKs, and editor integrations that speak OpenAI or Anthropic can point at
Lanius's `/v1/...` endpoints without any code changes, while Lanius handles
authentication, protocol translation, and the quirks of Kiro's actual
backend.

## Module map (`lanius-core`)

```mermaid
graph LR
    lib["lib.rs<br/>crate root; architecture overview + curated re-exports"]

    subgraph api["api/ — HTTP-facing routes for both client protocols"]
        subgraph api_openai["openai/ — /v1/models, /v1/chat/completions"]
            api_openai_models["models.rs<br/>serde wire types"]
            api_openai_routes["routes.rs<br/>axum handlers, OpenAiState"]
            api_openai_sse["sse.rs<br/>streaming encoder + non-streaming response builder"]
        end
        subgraph api_anthropic["anthropic/ — /v1/messages, /v1/messages/count_tokens"]
            api_anthropic_models["models.rs<br/>serde wire types"]
            api_anthropic_routes["routes.rs<br/>axum handlers, AnthropicState"]
            api_anthropic_sse["sse.rs<br/>streaming state machine + non-streaming response builder"]
        end
    end

    subgraph convert["convert/ — client wire format &lt;-&gt; Kiro payload conversion"]
        convert_core["core.rs<br/>provider-agnostic UnifiedMessage/UnifiedTool + build_kiro_payload"]
        convert_openai["openai.rs<br/>OpenAI request -&gt; UnifiedMessage/UnifiedTool"]
        convert_anthropic["anthropic.rs<br/>Anthropic request -&gt; UnifiedMessage/UnifiedTool"]
        convert_guards["guards.rs<br/>payload-size enforcement, tool-result repair after trimming"]
    end

    subgraph auth["auth/ — credential loading, token refresh, persistence"]
        auth_credentials["credentials.rs<br/>env / JSON file / kiro-cli SQLite loading"]
        auth_refresh["refresh.rs<br/>the actual refresh-token HTTP exchange"]
    end

    subgraph upstream["upstream/ — talking to Kiro over HTTP and decoding its response"]
        upstream_client["client.rs<br/>retry/backoff HTTP client (KiroHttpClient)"]
        upstream_parser["parser.rs<br/>AWS event-stream fragment decoder (AwsEventStreamParser)"]
        upstream_stream["stream.rs<br/>provider-agnostic KiroEvent stream + StreamResult collector"]
    end

    subgraph model["model/ — model catalog and name resolution"]
        model_cache["cache.rs<br/>ModelInfoCache (TTL'd catalog snapshot)"]
        model_resolver["resolver.rs<br/>ModelResolver (name normalization/aliasing)"]
        model_reasoning["reasoning.rs<br/>per-model native thinking/reasoning capability"]
    end

    compat["compat.rs<br/>client-compatibility hooks (tool-name aliasing, host rewriting, model-id formatting)"]
    truncation["truncation.rs<br/>detect + recover from upstream output truncation"]
    tokenizer["tokenizer.rs<br/>token estimation when Kiro doesn't report usage"]
    config["config.rs<br/>Config struct, env var parsing, defaults, URL templates"]
    error["error.rs<br/>GatewayError, Kiro/network error classification"]
    server["server.rs<br/>axum app assembly, AppState, serve()/spawn()"]
    utils["utils.rs<br/>fingerprinting, user-agent, id generation, spaced JSON"]

    lib --> api
    lib --> convert
    lib --> auth
    lib --> upstream
    lib --> model
    lib --> compat
    lib --> truncation
    lib --> tokenizer
    lib --> config
    lib --> error
    lib --> server
    lib --> utils
```

## Data flow

```mermaid
flowchart TD
    client["client<br/>(any OpenAI/Anthropic-compatible SDK/tool)"]
    server_router["server.rs — axum Router<br/>CORS (localhost only) · panic recovery ·<br/>tracing · model-id rewrite for Claude clients"]
    openai_routes["api::openai::routes<br/>(bearer PROXY_API_KEY check)"]
    anthropic_routes["api::anthropic::routes<br/>(x-api-key/anthropic-version check)"]
    convert_adapters["convert::{openai,anthropic}<br/>client message/tool shapes -&gt; UnifiedMessage/UnifiedTool"]
    build_payload["convert::core::build_kiro_payload<br/>role normalization · tool preprocessing · native reasoning fields ·<br/>convert::guards payload-size trimming/repair"]
    resolver["model::resolver::ModelResolver::resolve<br/>client model name -&gt; concrete Kiro model id"]
    auth_manager["auth::AuthManager<br/>ensures a fresh bearer token (refreshing if needed)"]
    kiro_client["upstream::client::KiroHttpClient<br/>POST generateAssistantResponse, retry/backoff, 403 -&gt; refresh"]
    aws_parser["upstream::parser::AwsEventStreamParser<br/>incremental JSON-fragment decoding -&gt; ParserEvent<br/>(AWS event-stream framed bytes)"]
    kiro_stream["upstream::stream::parse_kiro_stream<br/>ParserEvent -&gt; provider-agnostic KiroEvent (Content/Thinking/<br/>ToolUse/Usage/ContextUsage/Error)"]
    openai_sse["api::openai::sse<br/>(chunk/response)"]
    anthropic_sse["api::anthropic::sse<br/>(SSE state machine/response)"]
    truncation_store["truncation::TruncationStore<br/>(records/recovers from output cut off by Kiro's size limits)"]

    client -->|HTTP| server_router
    server_router --> openai_routes
    server_router --> anthropic_routes
    openai_routes --> convert_adapters
    anthropic_routes --> convert_adapters
    convert_adapters --> build_payload
    build_payload --> resolver
    resolver --> auth_manager
    auth_manager --> kiro_client
    kiro_client --> aws_parser
    aws_parser --> kiro_stream
    kiro_stream --> openai_sse
    kiro_stream --> anthropic_sse
    openai_sse --> truncation_store
    anthropic_sse --> truncation_store
    truncation_store --> client
```

## Cross-cutting concerns

- **`config`** — every tunable is an environment variable with a documented
  default (see [configuration.md](configuration.md)); `Config::validate`
  runs once at startup and refuses to serve with an insecure/invalid setup.
- **`error`** — a single `GatewayError` enum spans config, auth, network,
  and upstream-classified errors; `error::enhance_kiro_error` turns Kiro's
  raw error payloads into actionable messages (e.g. distinguishing "context
  length exceeded" from a generic 400).
- **`compat`** — behaviors that are neither "shape conversion" (`convert`)
  nor "credentials" (`auth`) but still apply to nearly every request: tool
  name aliasing for Kiro's 64-character name limit, host rewriting for
  control-plane-only operations, and Claude-client-specific model-id
  formatting. See [compatibility.md](compatibility.md).
- **`utils`** — byte-for-byte matching of Kiro's expected wire format
  (spaced JSON separators, specific header casing/ordering, a stable
  per-machine fingerprint) lives here since it's needed by both `convert`
  and `upstream`.

## Where to look next

- Tracing one request end-to-end: [request-flow.md](request-flow.md)
- How message/tool shapes are translated: [conversion.md](conversion.md)
- Credentials and token refresh: [auth.md](auth.md)
- Decoding Kiro's streaming response: [streaming.md](streaming.md)
