# Configuration

Lanius is configured entirely through environment variables (a `.env` file
in the working directory is also loaded automatically via `dotenvy`, if
present). `Config::from_env()` reads them once at startup, falling back to
`Config::default()` for anything unset or unparseable (a warning is logged
for unparseable values, not a hard failure). `Config::validate()` then runs
once before the server starts serving traffic and refuses to start on an
insecure/invalid configuration.

`crates/lanius-core/src/config.rs` is the single source of truth for every
default; this table mirrors it.

## Server

| Variable | Default | Purpose |
|---|---|---|
| `SERVER_HOST` | `0.0.0.0` | Bind address for the gateway's HTTP server. |
| `SERVER_PORT` | `18000` | Bind port. |
| `PROXY_API_KEY` | *(insecure placeholder — must be set)* | Bearer key clients must present to Lanius. Validation fails if empty/whitespace. |

## Kiro / AWS connection

| Variable | Default | Purpose |
|---|---|---|
| `KIRO_REGION` | `us-east-1` | AWS region used to build Kiro/OIDC endpoint URL templates. Validation fails if empty. |
| `VPN_PROXY_URL` | unset | Optional HTTP/HTTPS/SOCKS5 proxy for all upstream Kiro requests. Must start with `http://`, `https://`, `socks5://`, or `socks5h://`. |

## Credentials

| Variable | Default | Purpose |
|---|---|---|
| `REFRESH_TOKEN` | unset | Refresh token used to authenticate with Kiro (seeds `Credentials`; may be overridden by a file/SQLite source). |
| `PROFILE_ARN` | unset | Optional AWS IAM profile ARN associated with the refresh token. |
| `KIRO_CREDS_FILE` | unset | Path to a Kiro desktop JSON credentials file (`~` expanded). |
| `KIRO_CLI_DB_FILE` | unset | Path to the Kiro CLI's SQLite database, an alternate credential source that takes priority over `KIRO_CREDS_FILE` when both are set. |
| `SQLITE_READONLY` | `false` | When `true`, refreshed credentials are never written back to the SQLite database. |

See [auth.md](auth.md) for how these sources are merged and prioritized.

## Request handling

| Variable | Default | Purpose |
|---|---|---|
| `TOOL_DESCRIPTION_MAX_LENGTH` | `10000` | Max length of a tool description before it's moved into the system prompt instead (see [conversion.md](conversion.md)). |
| `TRUNCATION_RECOVERY` | `true` | Whether truncated tool/content output triggers the recovery-prompt flow (see [truncation.md](truncation.md)). |
| `KIRO_MAX_PAYLOAD_BYTES` | `600000` | Maximum request payload size, in bytes (Kiro's own counting method), sent upstream. |
| `AUTO_TRIM_PAYLOAD` | `false` | When `true`, oversized payloads are automatically trimmed (dropping oldest history) rather than rejected. |
| `WEB_SEARCH_ENABLED` | `true` | Whether the synthetic `web_search` tool is advertised/injected for OpenAI-style requests. |

## Timeouts and retries

| Variable | Default | Purpose |
|---|---|---|
| `FIRST_TOKEN_TIMEOUT` | `15` (seconds, float) | How long to wait for the first byte of a streaming response before failing. |
| `STREAMING_READ_TIMEOUT` | `300` (seconds, float) | How long to wait between subsequent chunks before failing a stall mid-stream. |
| `FIRST_TOKEN_MAX_RETRIES` | `3` | Max attempts made while waiting for the first streamed token. |

Non-streaming requests additionally use a fixed 300s total timeout and a
30s connect timeout (not configurable via environment variable; see
`upstream::client::NON_STREAM_REQUEST_TIMEOUT`/`CONNECT_TIMEOUT`).
`MAX_RETRIES` (3) and `BASE_RETRY_DELAY` (1s, exponential) govern the
403/429/5xx retry policy — also not currently environment-configurable.

## Logging and debugging

| Variable | Default | Purpose |
|---|---|---|
| `LOG_LEVEL` | `INFO` | `tracing` verbosity (upper-cased automatically). Honors `RUST_LOG` if set, taking priority. |
| `DEBUG_MODE` | `off` | `off` \| `errors` \| `all` — extra internal debug logging/dumping. Case-insensitive; unrecognized values fall back to `off`. |
| `DEBUG_DIR` | `debug_logs` | Directory debug dumps are written to when `DEBUG_MODE` is enabled. |

## Model aliases and hiding

`model_aliases` and `hidden_from_list` are not currently environment
variables — they're populated from `config::default_model_aliases()`
(`auto-kiro` -> `auto`) and `config::default_hidden_from_list()` (`["auto"]`)
respectively. See [model-catalog.md](model-catalog.md) for how they're
used.

## Validation rules

`Config::validate()` fails startup (returns `GatewayError::Config`) if:

- `PROXY_API_KEY` is empty or whitespace-only — clients could otherwise
  authenticate with any key at all.
- `KIRO_REGION` is empty or whitespace-only.
- `SERVER_PORT` is `0`.
- `VPN_PROXY_URL` is set but doesn't start with one of the four accepted
  schemes.

## URL templates

Every Kiro/AWS endpoint URL is built from a `{region}`-templated constant
in `config.rs`:

| Constant | Template | Used for |
|---|---|---|
| `KIRO_REFRESH_URL_TEMPLATE` | `https://prod.{region}.auth.desktop.kiro.dev/refreshToken` | Kiro desktop token refresh |
| `AWS_SSO_OIDC_URL_TEMPLATE` | `https://oidc.{region}.amazonaws.com/token` | AWS SSO OIDC token refresh |
| `KIRO_API_HOST_TEMPLATE` | `https://runtime.{region}.kiro.dev` | Chat/completion requests (paid runtime) |
| `KIRO_Q_HOST_TEMPLATE` | `https://runtime.{region}.kiro.dev` | Model listing, usage/account queries (control plane) |

These are exposed as `Config::refresh_url()`, `Config::oidc_url()`,
`Config::api_host()`, `Config::q_host()`,
`Config::generate_assistant_response_url()`, and
`Config::list_available_models_url()`. See
[compatibility.md](compatibility.md#host-rewriting) for the rules around
when the control-plane host is used instead of the runtime host.
