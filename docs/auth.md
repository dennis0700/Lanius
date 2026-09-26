# Authentication (`auth`)

`auth` is the gateway's single source of truth for "how do we prove who we
are to Kiro/AWS right now." It has three parts:

- **`auth::credentials`** — loads raw credentials from whichever source is
  configured.
- **`auth::refresh`** — performs the network call that exchanges a refresh
  token for a fresh access token.
- **`auth::AuthManager`** (in `auth.rs` itself) — owns the shared, mutable
  token state, decides when to refresh, retries in the face of external
  token rotation, and persists refreshed credentials back to disk.

There is normally one `AuthManager` per running gateway process, shared via
`Arc` in `AppState`/`OpenAiState`/`AnthropicState`.

## Credential sources and priority

`Credentials::load` (called once by `AuthManager::new`) merges sources in
this order, later sources overriding earlier ones where they provide a
value:

1. **Environment / config** — `REFRESH_TOKEN` and `PROFILE_ARN` seed the
   initial value.
2. **`kiro-cli` SQLite database** (`KIRO_CLI_DB_FILE`) — if configured, this
   takes priority over a JSON credentials file. `SQLITE_TOKEN_KEYS` defines
   the fallback order for which `auth_kv` row holds the live token (social
   login > OIDC device registration), and a companion
   `SQLITE_REGISTRATION_KEYS` lookup finds the OIDC client id/secret for the
   AWS SSO flow if that path is in use.
3. **JSON credentials file** (`KIRO_CREDS_FILE`) — consulted only if no
   SQLite database is configured. `merge_enterprise_registration` also
   reads a separate `~/.aws/sso/cache/*.json` file when the credentials
   file's `clientIdHash` field points at an enterprise device registration.

Everything in `Credentials` is `Option` because no single source populates
every field (a bare refresh-token setup has no `client_id`/`client_secret`
at all).

## Auth type

`AuthType` is derived, never configured directly:

- `AuthType::AwsSsoOidc` — both `client_id` and `client_secret` are present
  and non-empty (IAM Identity Center / enterprise accounts).
- `AuthType::KiroDesktop` — otherwise (the Kiro desktop app's own refresh
  flow: refresh token only, no client credentials).

## Token lifecycle

`AuthManager::access_token` (the method almost every request path calls) does:

1. **Fast path** — if the in-memory token is not within
   `TOKEN_REFRESH_THRESHOLD` (10 minutes) of expiry, return it with no I/O.
2. **SQLite peek** — if SQLite-backed and the token looks like it's
   expiring, reload from SQLite first. Another process (`kiro-cli` itself)
   may have already refreshed and written a newer token there, saving a
   network round trip and avoiding a race with that other writer.
3. **Network refresh** — otherwise, perform an actual refresh via
   `refresh::refresh`, which dispatches to the desktop or OIDC flow based
   on `AuthType`.

`access_token_and_autofetch` wraps this with a best-effort, at-most-once
profile ARN discovery (see below) using the token it just obtained; a
failure there is logged and swallowed, never surfaced as an error, since
it's an optimization rather than a hard requirement.

`force_refresh` bypasses the "is it still valid" fast path entirely — for
callers that already know the cached token was rejected upstream (e.g. got
a 401) and need a guaranteed-fresh one.

### SQLite HTTP-400 recovery

If a refresh network call fails with HTTP 400 *and* the credential source
is SQLite-backed, `AuthManager` assumes another process (e.g. `kiro-cli
login`) may have just rotated the refresh token out from under it. Before
giving up, it reloads from SQLite and retries once
(`refresh_locked`/`should_reload_oidc_after_http_400`); if that still
doesn't yield a token but the SQLite-loaded token happens to still be
unexpired, it's used anyway rather than hard-failing on a now-stale HTTP
400.

## Refresh flows (`auth::refresh`)

Two flows, selected by `AuthType`:

- **Kiro Desktop**: `POST {desktop_url}` with `{"refreshToken": ...}` —
  deliberately minimal, no client credentials or scope.
- **AWS SSO OIDC**: `POST {oidc_url}` with the OIDC `refresh_token` grant
  body (`grantType`, `clientId`, `clientSecret`, `refreshToken`) —
  deliberately omits any `scope`/`scopes` field, since the endpoint used
  here rejects requests that include one.

Both flows are invoked exclusively while `AuthManager` holds its state
lock, so at most one refresh is ever in flight per manager — concurrent
callers simply wait on the same lock rather than triggering redundant
refreshes.

**Security invariant**: every error path in `refresh.rs` surfaces only the
HTTP status code or a fixed, non-parameterized message — never the raw
response body — because that body may legitimately contain a freshly
issued (or about-to-be-superseded) access/refresh token. The same
discipline applies to `auth.rs`'s own error constructors (e.g.
`sqlite_refresh_failed_error`).

## Persistence

After a successful refresh, `AuthManager::apply_outcome` updates in-memory
state, then `persist` writes the updated credentials back to whichever
store is configured (SQLite takes priority over a JSON file; if neither is
configured, this is a no-op):

- **`persist_file`** — atomically rewrites the JSON credentials file
  (preserving any unrecognized existing fields), so a crash or concurrent
  read never observes a half-written file.
- **`persist_sqlite`** — writes back into the matched `auth_kv` row,
  preferring the key credentials were originally loaded from, then falling
  back through `SQLITE_TOKEN_KEYS`. Honors `SQLITE_READONLY=true` (skips
  the write, logged at debug level) and tolerates a missing/unopenable
  database file as a soft failure — the in-memory token is already updated
  and usable for the current process even if the on-disk copy couldn't be
  refreshed.

Persistence failures are always logged, never propagated as an error to
the request in flight — a failed write-back should not block returning a
perfectly valid, freshly refreshed token.

## Profile ARN autofetch

Kiro's paid "runtime" host requires a profile ARN; Builder ID accounts
without one otherwise fall back to the (unpaid) control-plane host for chat
too (see [compatibility.md](compatibility.md#host-rewriting)). If no
profile ARN is configured, `AuthManager::autofetch_profile_arn` tries, at
most once per process lifetime
(`compat::ProfileArnAutofetchHook::claim_fetch` atomically claims the
attempt), to discover one by calling Kiro's `ListAvailableProfiles`
control-plane API. Any failure (network, non-200, malformed JSON, no usable
profile in the response) is logged at `warn` and otherwise ignored.

## Region resolution

The effective API region (`AuthManager::region`) prefers, in order: a
region detected from a profile ARN, then the SSO region reported alongside
credentials, then the statically configured `KIRO_REGION` default
(`final_api_region`).

## Retry-and-refresh policy (upstream client)

`upstream::client::KiroHttpClient` layers its own retry policy on top of
this module: HTTP 403 triggers a forced token refresh
(`AuthManager::force_refresh`) before retrying, HTTP 429/5xx are retried
with exponential backoff (`BASE_RETRY_DELAY * 2^attempt`, capped at
`MAX_RETRIES`), and transport-level failures are retried only when
`error::classify_network_error` marks them retryable. See
[architecture.md](architecture.md) for where this sits in the overall
request flow.
