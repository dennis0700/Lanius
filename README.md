English | [简体中文](README.zh-CN.md)

# Lanius

An OpenAI- and Anthropic-compatible API gateway for the Kiro backend
(Amazon Q Developer / AWS CodeWhisperer). Lanius lets tools and clients built
against the OpenAI Chat Completions API or the Anthropic Messages API talk
to Kiro without knowing anything about Kiro's own protocol.

![Lanius desktop app](desktop-ui.png)

## Workspace layout

This is a Cargo workspace with three crates:

| Crate | Description |
|---|---|
| [`crates/lanius-core`](crates/lanius-core) | The gateway itself: HTTP server, request/response conversion, auth, streaming parser, and all business logic. A library crate consumed by both `lanius-cli` and `lanius-gui`. |
| [`crates/lanius-cli`](crates/lanius-cli) | Headless binary (`lanius`) for running the gateway as a server process, replaying captured streams offline, and probing the live Kiro backend for debugging. |
| [`crates/lanius-gui`](crates/lanius-gui) | Desktop app (`lanius-desktop`, built with [Slint](https://slint.dev)) that runs the gateway in-process, with a UI for configuration, live logs, model list, and usage. |

### Request flow (`lanius-core`)

```mermaid
flowchart TD
    client["client (OpenAI/Anthropic SDK)"]
    api["api::openai / api::anthropic<br/>(routes, request/response models, SSE framing)"]
    convert["convert<br/>(client format -&gt; unified internal representation -&gt; Kiro payload)"]
    auth["auth<br/>(token acquisition/refresh, credential loading)"]
    upstream["upstream<br/>(HTTP client with retry, AWS event-stream parsing)"]
    sse["convert / api::*::sse<br/>(Kiro stream events -&gt; OpenAI/Anthropic wire format)"]

    client --> api
    api --> convert
    convert --> auth
    auth --> upstream
    upstream --> sse
    sse --> client
```

Supporting modules: `compat` (per-client compatibility hooks, e.g. tool-name
aliasing and model-id rewriting), `model` (model catalog cache, name
resolution, and per-model native thinking/reasoning support read from the
catalog schema), `truncation` (recovery from upstream tool-call/content
truncation), `tokenizer`
(token estimation), and `config`/`error` (configuration and unified error
types).

See each module's `//!` doc comment (e.g. `cargo doc --open -p lanius-core`)
for details on individual components, or the [`docs/`](docs) directory for
higher-level, topic-by-topic write-ups (architecture, request flow,
conversion, auth, streaming, compatibility hooks, model resolution,
truncation recovery, configuration reference, the desktop GUI, and the test
suite). Start at [`docs/README.md`](docs/README.md).

## Building

Requires a recent stable Rust toolchain (edition 2024, `rust-version = "1.85"`).

```sh
# Build everything
cargo build --workspace

# Build/run just the headless server
cargo run -p lanius-cli

# Build the desktop app (requires Slint's platform dependencies)
cargo run -p lanius-gui
```

## Running the gateway

The CLI binary is `lanius`:

```sh
lanius                          # validate config and start serving
lanius probe [prompt]           # live end-to-end request against the Kiro backend
lanius probe --capture out.bin  # same, saving the raw upstream stream to a file
lanius replay out.bin           # offline-decode a previously captured raw stream
lanius usage                    # show the configured account's credit/quota usage
lanius usage --json             # same, as raw Kiro JSON
lanius models                   # list models currently resolvable by this gateway
lanius models --json            # same, as JSON
lanius --version                # print the version and exit
lanius help
```

Configuration is read from the environment (a `.env` file is also loaded if
present). Key variables — see [`docs/configuration.md`](docs/configuration.md)
or [`crates/lanius-core/src/config.rs`](crates/lanius-core/src/config.rs)
for the full, authoritative list and defaults:

| Variable | Purpose |
|---|---|
| `PROXY_API_KEY` | Bearer key clients must present to Lanius. Required, must not be empty. |
| `KIRO_REGION` | AWS region used to build Kiro/OIDC endpoint URLs. |
| `SERVER_HOST` / `SERVER_PORT` | Bind address for the gateway's HTTP server. |
| `REFRESH_TOKEN` / `PROFILE_ARN` | Refresh token and (optional) profile ARN used to authenticate with Kiro. |
| `KIRO_CREDS_FILE` / `KIRO_CLI_DB_FILE` | Alternative credential sources (JSON file / Kiro CLI's SQLite DB). |
| `LOG_LEVEL` / `DEBUG_MODE` | Logging verbosity and optional request/response debug capture. |
| `WEB_SEARCH_ENABLED` | Whether OpenAI-style requests get a server-executed `web_search` tool (default `true`). |

The gateway exposes:

- `GET /v1/models`, `POST /v1/chat/completions` — OpenAI-compatible API
- `POST /v1/messages`, `POST /v1/messages/count_tokens` — Anthropic-compatible API
- `GET /usage`, `GET /account` — usage/quota for the configured account, gated by `PROXY_API_KEY`
- `GET /health` — unauthenticated health check

All routes other than `/health` require the `PROXY_API_KEY` bearer token, and
CORS is restricted to `localhost`/`127.0.0.1` origins.

### Web search

Web search is backed by Kiro's native search, the same one Kiro CLI uses:

- **Claude Code / Cowork**: the built-in WebSearch tool works. Its search
  sub-requests are answered directly with `server_tool_use` +
  `web_search_tool_result` blocks.
- **Anthropic API**: requests that offer the server tool
  (`web_search_20250305`) get a `web_search` tool that Lanius runs on the
  model's behalf, honoring `max_uses` and reporting
  `usage.server_tool_use.web_search_requests`.
- **OpenAI API**: a `web_search` tool is offered (unless the client defines
  its own) and executed by Lanius, so clients only see the final answer.
  Disable with `WEB_SEARCH_ENABLED=false`.

For Kiro enterprise accounts, Lanius honors the console's "Web search and
web fetch tools" switch: when it is off, no search tool is offered.
See [`docs/request-flow.md`](docs/request-flow.md#server-side-web-search).

### Context compaction

Claude clients compact long conversations automatically. Lanius reports
Kiro's actual context usage as `input_tokens` in the streamed
`message_delta`, and returns Kiro's context-limit rejection as Anthropic's
`400 invalid_request_error` "prompt is too long: N tokens > M maximum", the
error Claude clients compact on.

### Network access

To use Lanius behind a firewall or proxy, allow these hosts (all are on
Kiro's [firewall allowlist](https://kiro.dev/docs/privacy-and-security/firewalls/)
except GitHub):

| Host | Used for |
|---|---|
| `runtime.<region>.kiro.dev` | Chat and web search |
| `management.<region>.kiro.dev` | Profiles, model list, usage, enterprise policy |
| `prod.<region>.auth.desktop.kiro.dev` | Token refresh (social / Builder ID login) |
| `oidc.<region>.amazonaws.com` | Token refresh (IAM Identity Center) |
| `api.github.com`, `github.com`, `release-assets.githubusercontent.com` | Update checks and downloads only |

`VPN_PROXY_URL` routes the `runtime`/`management` traffic through an HTTP or
SOCKS proxy; token refresh connects directly.

### Deploying on Linux (systemd)

Prebuilt static Linux binaries (amd64/arm64) are attached to each GitHub
Release. See [`docs/deployment.md`](docs/deployment.md) for installing
`lanius` and running it as a hardened systemd service.

## Updating

- **Desktop app**: checks GitHub for a newer release automatically (toggle in
  Settings), or click "Check Now" there to check on demand. When a newer
  version is found, click "Update & Restart" to download, verify, and
  install it. See [`docs/gui.md`](docs/gui.md) for how this works under the
  hood. The Windows build (`Lanius-<version>-windows-x64.zip`, portable —
  unzip and run) also notifies you of new releases, but doesn't install in
  place yet: "Update & Restart" opens the release page for a manual
  download.
- **CLI (`lanius`)**: run `lanius update` to check for and install the
  latest release in place (`lanius update --check` to only check,
  `lanius update -y` to skip the confirmation prompt). See
  [`docs/deployment.md`](docs/deployment.md#upgrading) for the systemd
  upgrade workflow.
- **Building from source**: `git pull`, then rebuild with
  `cargo build --workspace` (or `-p lanius-cli` / `-p lanius-gui`).

## Testing

```sh
cargo test --workspace
```

Includes unit tests across `lanius-core` and `lanius-gui`, plus
`lanius-cli`'s GUI/desktop config-mapping contract tests.

## License

AGPL-3.0 — see [LICENSE](LICENSE).
