//! Authentication and token-lifecycle management for the Kiro backend.
//!
//! This module is the gateway's single source of truth for "how do we prove who we are
//! to Kiro/AWS right now". It ties together three concerns that live in submodules:
//!
//! - `credentials` — loading raw credentials (access/refresh tokens, OIDC client
//!   id/secret, profile ARN, region) from the environment, a JSON credentials file, or a
//!   `kiro-cli` SQLite database, with a defined priority order.
//! - `refresh` — performing the actual network call that exchanges a refresh token for
//!   a fresh access token, for either the "Kiro Desktop" flow or the AWS SSO OIDC flow.
//! - [`AuthManager`] (this file) — owning the mutable, shared `TokenState` behind a
//!   [`tokio::sync::Mutex`], deciding *when* a refresh is needed, retrying refreshes that
//!   fail because on-disk SQLite credentials were rotated out from under us, persisting
//!   refreshed credentials back to disk, and best-effort auto-fetching a Kiro "profile
//!   ARN" the first time a token is used without one.
//!
//! Downstream, [`crate::server`] and the `api::*` request handlers call
//! [`AuthManager::access_token`] / [`AuthManager::access_token_and_autofetch`] to obtain a
//! bearer token before proxying a request upstream, and [`AuthManager::api_host`] /
//! [`AuthManager::control_plane_host`] (which delegate to [`crate::compat`]'s host-rewrite
//! hooks) to decide which Kiro/AWS endpoint to call. [`crate::compat::ProfileArnAutofetchHook`]
//! is reused here (rather than duplicated) so the "fetch a profile ARN at most once" logic
//! has a single implementation.
//!
//! Security note: this module handles live access/refresh tokens and OIDC client secrets.
//! Errors constructed here are careful to never embed raw upstream response bodies (which
//! may contain tokens) in messages surfaced to callers — see `sqlite_refresh_failed_error`
//! and `refresh::into_result`.

mod credentials;
mod refresh;

use std::fs;
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::compat::{self, ProfileArnAutofetchHook};
use crate::config::{Config, TOKEN_REFRESH_THRESHOLD};
use crate::error::{GatewayError, Result};
use crate::utils::machine_fingerprint;

use credentials::Credentials;
use refresh::{RefreshEndpoints, RefreshError, RefreshOutcome};

/// Which refresh-token flow a set of credentials should use.
///
/// This is derived (never configured directly) from whether an OIDC client id/secret pair
/// is present: see `AuthType::from_credentials`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    /// The Kiro desktop app's own refresh endpoint (refresh token only, no client
    /// id/secret).
    KiroDesktop,
    /// The AWS SSO OIDC `refresh_token` grant, which additionally requires a client id
    /// and client secret registered for this device.
    AwsSsoOidc,
}

impl AuthType {
    /// Picks [`AuthType::AwsSsoOidc`] when both `client_id` and `client_secret` are present
    /// and non-empty; otherwise falls back to [`AuthType::KiroDesktop`].
    fn from_credentials(credentials: &Credentials) -> Self {
        if credentials
            .client_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            && credentials
                .client_secret
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        {
            Self::AwsSsoOidc
        } else {
            Self::KiroDesktop
        }
    }
}

/// The mutable, in-memory authentication state guarded by [`AuthManager::state`].
///
/// Everything here can change as a result of a token refresh or a SQLite reload, so it is
/// always accessed through the manager's mutex rather than copied out and cached.
#[derive(Debug)]
struct TokenState {
    credentials: Credentials,
    auth_type: AuthType,
    region: String,
}

/// Owns the current credentials for one gateway process and coordinates refreshing,
/// persisting, and reading them.
///
/// There is normally one `AuthManager` per running gateway. All async methods that touch
/// `TokenState` take the internal [`tokio::sync::Mutex`] lock, so concurrent callers are
/// serialized: only one refresh (network request) happens at a time, and callers that ask
/// for a token while a refresh is already in flight simply wait for the same lock rather
/// than triggering a second, redundant refresh.
pub struct AuthManager {
    config: Config,
    fingerprint: String,
    state: Mutex<TokenState>,
    profile_autofetch: ProfileArnAutofetchHook,
}

impl AuthManager {
    /// Builds a new manager by loading credentials per `Credentials::load` (environment,
    /// then credentials file or SQLite database) and deriving the initial [`AuthType`] and
    /// API region from them.
    ///
    /// This does not perform any network I/O; it only reads local configuration/files.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// let manager = AuthManager::new(Config::default());
    /// assert!(manager.is_ok());
    /// ```
    pub fn new(config: Config) -> Result<Self> {
        let credentials = Credentials::load(&config)?;
        let region = final_api_region(&config, &credentials);
        let auth_type = AuthType::from_credentials(&credentials);
        Ok(Self {
            config,
            fingerprint: machine_fingerprint().to_string(),
            state: Mutex::new(TokenState {
                credentials,
                auth_type,
                region,
            }),
            profile_autofetch: ProfileArnAutofetchHook::default(),
        })
    }

    /// Returns a currently-valid access token, refreshing it first if necessary.
    ///
    /// Order of operations while holding the state lock:
    /// 1. If the in-memory token is not close to expiring, return it immediately (no I/O).
    /// 2. If we're backed by a `kiro-cli` SQLite database and the token looks like it is
    ///    expiring, reload from SQLite first — another process (the `kiro-cli` itself) may
    ///    have already refreshed and written a newer token there, which saves a network
    ///    round trip and avoids racing with that other writer.
    /// 3. Otherwise perform an actual network refresh via `refresh::refresh`.
    ///
    /// One SQLite-specific recovery path: if the refresh network call fails with HTTP 400
    /// and we are SQLite-backed, we tolerate a still-valid (not yet expired, just within
    /// the "expiring soon" threshold) token from SQLite rather than hard-failing — this
    /// covers the case where the CLI just rotated the refresh token itself, so our HTTP 400
    /// is stale rather than a real auth failure.
    ///
    /// May perform a network request. Never logs or returns raw token values from the
    /// upstream error body.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// let token = manager.access_token().await;
    /// # }
    /// ```
    pub async fn access_token(&self) -> Result<String> {
        let mut state = self.state.lock().await;
        if let Some(token) = valid_token(&state.credentials) {
            return Ok(token.to_string());
        }

        if self.config.kiro_cli_db_file.is_some() && is_expiring(&state.credentials.expires_at) {
            self.reload_sqlite(&mut state)?;
            if let Some(token) = valid_token(&state.credentials) {
                return Ok(token.to_string());
            }
        }

        match self.refresh_locked(&mut state).await {
            Ok(token) => Ok(token),
            Err(RefreshError::HttpStatus(status))
                if status.as_u16() == 400 && self.config.kiro_cli_db_file.is_some() =>
            {
                if let Some(token) = state.credentials.access_token.as_deref() {
                    if !is_expired(&state.credentials.expires_at) {
                        tracing::warn!(
                            "using still-valid SQLite access token after refresh HTTP 400"
                        );
                        return Ok(token.to_string());
                    }
                }
                Err(sqlite_refresh_failed_error(status))
            }
            Err(error) => refresh::into_result(error).and_then(|()| {
                Err(GatewayError::Internal(
                    "unreachable refresh success branch".to_string(),
                ))
            }),
        }
    }

    /// Unconditionally performs a network token refresh, bypassing the "is it still
    /// valid" fast path used by [`AuthManager::access_token`].
    ///
    /// Intended for callers that already know the current token was rejected by upstream
    /// (e.g. got a 401 from Kiro) and need a guaranteed-fresh token, not just a cached one.
    /// Always performs a network request.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// let token = manager.force_refresh().await;
    /// # }
    /// ```
    pub async fn force_refresh(&self) -> Result<String> {
        let mut state = self.state.lock().await;
        match self.refresh_locked(&mut state).await {
            Ok(token) => Ok(token),
            Err(error) => refresh::into_result(error).and_then(|()| {
                Err(GatewayError::Internal(
                    "unreachable refresh success branch".to_string(),
                ))
            }),
        }
    }

    /// Returns the currently known Kiro profile ARN, if any has been loaded or
    /// auto-fetched. Does not perform any I/O.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// let arn = manager.profile_arn().await;
    /// # }
    /// ```
    pub async fn profile_arn(&self) -> Option<String> {
        self.state.lock().await.credentials.profile_arn.clone()
    }

    /// Convenience wrapper around [`AuthManager::access_token`] that also kicks off a
    /// best-effort, at-most-once profile ARN auto-fetch (see
    /// `AuthManager::autofetch_profile_arn`) using the token it just obtained.
    ///
    /// May perform up to two network requests (token refresh, then the profile lookup).
    /// The profile auto-fetch failure is logged and swallowed — it never turns into an
    /// error for the caller, since it is an optimization rather than a requirement.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// let token = manager.access_token_and_autofetch().await;
    /// # }
    /// ```
    pub async fn access_token_and_autofetch(&self) -> Result<String> {
        let token = self.access_token().await?;
        self.autofetch_profile_arn(&token).await;
        Ok(token)
    }

    /// Attempts, at most once per `AuthManager` lifetime, to discover and persist a Kiro
    /// profile ARN by calling the `ListAvailableProfiles` control-plane API.
    ///
    /// The "at most once" guarantee comes from [`ProfileArnAutofetchHook::claim_fetch`],
    /// which atomically claims the attempt; this method is safe to call repeatedly (e.g.
    /// once per request) without triggering redundant network calls once a profile is
    /// known or an attempt has already been made. Any failure (network, non-200 status,
    /// malformed JSON, no usable profile in the response) is logged at `warn` level and
    /// otherwise ignored — this path is a convenience, not a hard dependency.
    async fn autofetch_profile_arn(&self, token: &str) {
        let profile_present = self
            .profile_arn()
            .await
            .is_some_and(|profile| !profile.is_empty());
        if !self
            .profile_autofetch
            .claim_fetch(!token.is_empty(), profile_present)
        {
            return;
        }
        let region = {
            let state = self.state.lock().await;
            state
                .credentials
                .sso_region
                .clone()
                .unwrap_or_else(|| state.region.clone())
        };
        let body = br#"{"nextToken": null}"#.to_vec();
        let headers = ProfileArnAutofetchHook::profile_headers(token, &self.fingerprint);
        let url = ProfileArnAutofetchHook::profile_url(&region);
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
        if let Some(proxy_url) = self
            .config
            .vpn_proxy_url
            .as_deref()
            .filter(|url| !url.is_empty())
        {
            match reqwest::Proxy::all(proxy_url) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(error) => {
                    tracing::warn!(error = %error, "profile ARN autofetch proxy is invalid");
                    return;
                }
            }
        }
        let client = match builder.build() {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(error = %error, "profile ARN autofetch client could not be built");
                return;
            }
        };
        let mut request = client.post(url).body(body);
        for (name, value) in &headers {
            request = request.header(name, value);
        }
        let response = match request.send().await {
            Ok(response) if response.status() == reqwest::StatusCode::OK => response,
            Ok(response) => {
                tracing::warn!(status = %response.status(), "profile ARN autofetch returned non-success status");
                return;
            }
            Err(error) => {
                tracing::warn!(error = %error, "profile ARN autofetch request failed");
                return;
            }
        };
        let value = match response.json::<Value>().await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(error = %error, "profile ARN autofetch returned invalid JSON");
                return;
            }
        };
        let Some(profile_arn) = ProfileArnAutofetchHook::profile_from_response(&value) else {
            tracing::warn!("profile ARN autofetch returned no usable profile");
            return;
        };
        let credentials = {
            let mut state = self.state.lock().await;
            // Re-check under the lock: another concurrent caller may have already set a
            // profile ARN (e.g. via a config-provided value or a race with another
            // autofetch) between our earlier unlocked read and now.
            if state
                .credentials
                .profile_arn
                .as_deref()
                .is_some_and(|profile| !profile.is_empty())
            {
                return;
            }
            state.credentials.profile_arn = Some(profile_arn);
            state.credentials.clone()
        };
        if self.config.kiro_creds_file.is_some() {
            if let Err(error) = self.persist_file(&credentials) {
                tracing::warn!(error = %error, "failed to persist autofetched profile ARN");
            }
        }
    }

    /// Returns the currently active API region (from detected profile ARN region, SSO
    /// region, or the configured default region, in that priority order — see
    /// `final_api_region`). Does not perform any I/O.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// let region = manager.region().await;
    /// assert!(!region.is_empty());
    /// # }
    /// ```
    pub async fn region(&self) -> String {
        self.state.lock().await.region.clone()
    }

    /// Returns the stable per-machine fingerprint used in outbound `User-Agent` headers.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// assert_eq!(manager.fingerprint().len(), 64, "SHA-256 hex digest is 64 chars");
    /// ```
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Returns the [`AuthType`] currently in effect for these credentials.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// let auth_type = manager.auth_type().await;
    /// # }
    /// ```
    pub async fn auth_type(&self) -> AuthType {
        self.state.lock().await.auth_type
    }

    /// Returns the fully-resolved chat/runtime API host for the current region and
    /// profile, applying [`compat::chat_host`]'s fallback rule (profile-less requests are
    /// routed to the control-plane host instead of the paid runtime host).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// let host = manager.api_host().await;
    /// assert!(host.starts_with("https://"));
    /// # }
    /// ```
    pub async fn api_host(&self) -> String {
        let region = self.region().await;
        let raw = crate::config::KIRO_API_HOST_TEMPLATE.replace("{region}", &region);
        compat::chat_host(&raw, self.profile_arn().await.as_deref())
    }

    /// Returns the control-plane (AWS `q.<region>.amazonaws.com`) host for the current
    /// region, used for account/profile-management calls that are never routed through the
    /// paid runtime host.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::auth::AuthManager;
    /// use lanius_core::Config;
    ///
    /// # async fn example() {
    /// let manager = AuthManager::new(Config::default()).expect("valid config");
    /// let host = manager.control_plane_host().await;
    /// assert!(host.contains("amazonaws.com"));
    /// # }
    /// ```
    pub async fn control_plane_host(&self) -> String {
        let region = self.region().await;
        let raw = crate::config::KIRO_Q_HOST_TEMPLATE.replace("{region}", &region);
        compat::control_plane_host(&raw)
    }

    /// Performs one refresh attempt while the state lock is held, with a single retry path:
    /// if refreshing OIDC credentials fails with HTTP 400 and we have a SQLite-backed
    /// credential source, we assume the SQLite file may have been rotated by another
    /// process (e.g. `kiro-cli login`) and reload it before retrying once. This avoids
    /// treating a stale in-memory refresh token as a hard failure when a fresher one is
    /// already on disk.
    async fn refresh_locked(
        &self,
        state: &mut TokenState,
    ) -> std::result::Result<String, RefreshError> {
        let endpoints = self.endpoints(state);
        let first = refresh::refresh(
            state.auth_type,
            &state.credentials,
            &endpoints,
            &self.fingerprint,
        )
        .await;

        let outcome = match first {
            Err(RefreshError::HttpStatus(status))
                if should_reload_oidc_after_http_400(
                    state.auth_type,
                    self.config.kiro_cli_db_file.is_some(),
                    status,
                ) =>
            {
                self.reload_sqlite(state).map_err(RefreshError::Gateway)?;
                let retry_endpoints = self.endpoints(state);
                refresh::refresh(
                    state.auth_type,
                    &state.credentials,
                    &retry_endpoints,
                    &self.fingerprint,
                )
                .await?
            }
            Err(error) => return Err(error),
            Ok(outcome) => outcome,
        };
        self.apply_outcome(state, outcome)?;
        state.credentials.access_token.clone().ok_or_else(|| {
            RefreshError::Gateway(GatewayError::Auth("Failed to obtain access token".into()))
        })
    }

    /// Re-reads credentials from the configured `kiro-cli` SQLite database, if any is
    /// configured, and re-derives [`AuthType`] and region from the merged result. A no-op
    /// (returns `Ok(())`) when no SQLite database is configured.
    fn reload_sqlite(&self, state: &mut TokenState) -> Result<()> {
        let Some(path) = self.config.kiro_cli_db_file.as_deref() else {
            return Ok(());
        };
        state.credentials.merge_sqlite(path)?;
        state.auth_type = AuthType::from_credentials(&state.credentials);
        state.region = final_api_region(&self.config, &state.credentials);
        Ok(())
    }

    /// Builds the desktop/OIDC refresh endpoint URLs for the current SSO region (falling
    /// back to the configured default region when the credentials do not specify one).
    fn endpoints(&self, state: &TokenState) -> RefreshEndpoints {
        let sso_region = state
            .credentials
            .sso_region
            .as_deref()
            .unwrap_or(&self.config.region);
        RefreshEndpoints {
            desktop_url: crate::config::KIRO_REFRESH_URL_TEMPLATE.replace("{region}", sso_region),
            oidc_url: crate::config::AWS_SSO_OIDC_URL_TEMPLATE.replace("{region}", sso_region),
        }
    }

    /// Applies a successful [`RefreshOutcome`] to the in-memory state (updating the access
    /// token always, and the refresh token/profile ARN only when the response provided a
    /// new value), then persists the updated credentials to whichever backing store is
    /// configured.
    fn apply_outcome(&self, state: &mut TokenState, outcome: RefreshOutcome) -> Result<()> {
        state.credentials.access_token = Some(outcome.access_token);
        if let Some(refresh_token) = outcome.refresh_token {
            state.credentials.refresh_token = Some(refresh_token);
        }
        if let Some(profile_arn) = outcome.profile_arn {
            state.credentials.profile_arn = Some(profile_arn);
        }
        state.credentials.expires_at = Some(outcome.expires_at);
        self.persist(&state.credentials)
    }

    /// Persists refreshed credentials to the configured backing store (SQLite takes
    /// priority over a JSON credentials file; if neither is configured, this is a no-op).
    /// Persistence failures are logged but never surfaced as an error to the caller — a
    /// failed write-back should not block returning a perfectly valid, freshly refreshed
    /// token to the request in flight.
    fn persist(&self, credentials: &Credentials) -> Result<()> {
        let result = if self.config.kiro_cli_db_file.is_some() {
            self.persist_sqlite(credentials)
        } else if self.config.kiro_creds_file.is_some() {
            self.persist_file(credentials)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            tracing::error!(%error, "failed to persist refreshed credentials");
        }
        Ok(())
    }

    /// Writes updated credential fields into the JSON credentials file, preserving any
    /// unrecognized existing fields in that file. The write is atomic (see
    /// [`atomic_write_json`]) so a crash or concurrent read never observes a half-written
    /// file. A no-op if no credentials file path is configured.
    fn persist_file(&self, credentials: &Credentials) -> Result<()> {
        let Some(path) = Credentials::source_path(&self.config) else {
            return Ok(());
        };
        let mut object = existing_json_object(&path)?;
        object.insert(
            "accessToken".to_string(),
            option_string(&credentials.access_token),
        );
        object.insert(
            "refreshToken".to_string(),
            option_string(&credentials.refresh_token),
        );
        if let Some(expires_at) = credentials.expires_at {
            object.insert(
                "expiresAt".to_string(),
                Value::String(expires_at.to_rfc3339()),
            );
        }
        if let Some(profile_arn) = &credentials.profile_arn {
            object.insert("profileArn".to_string(), Value::String(profile_arn.clone()));
        }
        atomic_write_json(&path, &Value::Object(object))
    }

    /// Writes updated credential fields back into the `kiro-cli` SQLite database's
    /// `auth_kv` table, preserving any unrecognized existing JSON fields under the matched
    /// key. A no-op when SQLite write-back is disabled via configuration
    /// (`sqlite_readonly`), the database file does not exist, or no matching token key is
    /// found. Open failures are logged and treated as a soft failure rather than
    /// propagated, since a stale on-disk token is not fatal — the in-memory token is
    /// already updated and usable for the current process.
    fn persist_sqlite(&self, credentials: &Credentials) -> Result<()> {
        if self.config.sqlite_readonly {
            tracing::debug!("SQLite write-back disabled (SQLITE_READONLY=true)");
            return Ok(());
        }
        let Some(path) = self.config.kiro_cli_db_file.as_deref() else {
            return Ok(());
        };
        if !path.exists() {
            tracing::warn!(path = %path.display(), "SQLite credential database not found for write-back");
            return Ok(());
        }
        let connection = match rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        ) {
            Ok(connection) => connection,
            Err(error) => {
                tracing::error!(%error, "failed to open SQLite credentials for write-back");
                return Ok(());
            }
        };
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| credentials::sqlite_error(&error))?;
        // Try the key the credentials were originally loaded from first (if any), then
        // fall back through the known SQLite token keys in priority order, so we update
        // whichever row is actually authoritative rather than always writing the first key.
        let preferred = credentials.sqlite_token_key.as_deref();
        let keys = preferred
            .into_iter()
            .chain(credentials::SQLITE_TOKEN_KEYS.iter().copied())
            .collect::<Vec<_>>();
        for key in keys {
            if merge_and_save_sqlite_key(&connection, key, credentials, &self.config.region)? {
                return Ok(());
            }
        }
        tracing::warn!("no matching SQLite token key found for write-back");
        Ok(())
    }
}

/// Determines the effective API region, preferring a region detected from a profile ARN,
/// then a region reported alongside the SSO credentials, and finally the statically
/// configured default region.
fn final_api_region(config: &Config, credentials: &Credentials) -> String {
    credentials
        .detected_api_region
        .clone()
        .or_else(|| credentials.sso_region.clone())
        .unwrap_or_else(|| config.region.clone())
}

/// Returns `true` when `expires_at` is missing or within [`TOKEN_REFRESH_THRESHOLD`] of
/// now — i.e. the token should be proactively refreshed even though it has not technically
/// expired yet.
fn is_expiring(expires_at: &Option<DateTime<Utc>>) -> bool {
    expires_at
        .map(|expires_at| {
            expires_at
                <= Utc::now()
                    + chrono::Duration::from_std(TOKEN_REFRESH_THRESHOLD).unwrap_or_default()
        })
        .unwrap_or(true)
}

/// Returns `true` when `expires_at` is missing or strictly in the past — i.e. the token is
/// truly unusable, as opposed to merely "expiring soon" (see [`is_expiring`]).
fn is_expired(expires_at: &Option<DateTime<Utc>>) -> bool {
    expires_at
        .map(|expires_at| expires_at <= Utc::now())
        .unwrap_or(true)
}

/// Returns the current access token only if it is non-empty and not [`is_expiring`].
fn valid_token(credentials: &Credentials) -> Option<&str> {
    credentials
        .access_token
        .as_deref()
        .filter(|token| !token.is_empty() && !is_expiring(&credentials.expires_at))
}

fn option_string(value: &Option<String>) -> Value {
    value.clone().map(Value::String).unwrap_or(Value::Null)
}

/// Reads and parses an existing JSON credentials file into its top-level object, or
/// returns an empty object if the file does not exist yet. Errors if the file exists but
/// its root is not a JSON object.
fn existing_json_object(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let raw = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&raw)?;
    let Value::Object(object) = value else {
        return Err(GatewayError::Auth(format!(
            "credentials file {} root is not an object",
            path.display()
        )));
    };
    Ok(object)
}

/// Builds a user-facing error for a refresh failure that returned HTTP 400 even after the
/// SQLite-reload retry. Intentionally includes only the HTTP status code — never the
/// upstream response body, which may contain a still-valid or freshly-issued token.
fn sqlite_refresh_failed_error(status: reqwest::StatusCode) -> GatewayError {
    GatewayError::Auth(format!(
        "Token expired and refresh failed. Please run 'kiro-cli login' to refresh your credentials. (HTTP {})",
        status.as_u16()
    ))
}

/// Whether an HTTP 400 refresh failure should trigger a SQLite reload-and-retry: only for
/// AWS SSO OIDC credentials, and only when a SQLite credential source is actually
/// configured (there is nothing to reload otherwise).
fn should_reload_oidc_after_http_400(
    auth_type: AuthType,
    has_sqlite_credentials: bool,
    status: reqwest::StatusCode,
) -> bool {
    auth_type == AuthType::AwsSsoOidc && has_sqlite_credentials && status.as_u16() == 400
}

/// Writes `value` to `path` atomically: serialize to a sibling temp file with restrictive
/// (owner-only, `0600` on Unix) permissions, then rename it into place. This ensures
/// concurrent readers never observe a partially-written credentials file, and that the
/// file is never briefly world- or group-readable while it contains secrets. On any
/// failure, the temp file is best-effort removed before the error is propagated.
pub(crate) fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            GatewayError::Auth(format!(
                "credentials path {} has no valid file name",
                path.display()
            ))
        })?;
    let temp = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    if let Err(error) = write_private_temp_file(&temp, &bytes) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

/// Creates `path` exclusively (fails if it already exists) with `0600` permissions on Unix
/// before writing `bytes`, so the temporary file never has looser permissions than the
/// final credentials file, even momentarily.
fn write_private_temp_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

/// Looks up `key` in the SQLite `auth_kv` table, merges the refreshed token fields into its
/// existing JSON value (preserving unknown fields), and writes the merged JSON back.
/// Returns `Ok(false)` (rather than an error) when the key does not exist or its value is
/// not a JSON object, so callers can fall through to try the next candidate key.
fn merge_and_save_sqlite_key(
    connection: &rusqlite::Connection,
    key: &str,
    credentials: &Credentials,
    default_region: &str,
) -> Result<bool> {
    let raw = match connection.query_row("SELECT value FROM auth_kv WHERE key = ?", [key], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(raw) => raw,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut object = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(object)) => object,
        _ => return Ok(false),
    };
    object.insert(
        "access_token".to_string(),
        option_string(&credentials.access_token),
    );
    object.insert(
        "refresh_token".to_string(),
        option_string(&credentials.refresh_token),
    );
    object.insert(
        "expires_at".to_string(),
        credentials
            .expires_at
            .map(|time| Value::String(time.to_rfc3339()))
            .unwrap_or(Value::Null),
    );
    object.insert(
        "region".to_string(),
        Value::String(
            credentials
                .sso_region
                .clone()
                .unwrap_or_else(|| default_region.to_string()),
        ),
    );
    if let Some(scopes) = &credentials.scopes {
        object.insert("scopes".to_string(), scopes.clone());
    }
    let merged = serde_json::to_string(&Value::Object(object))?;
    let changed = connection.execute(
        "UPDATE auth_kv SET value = ? WHERE key = ?",
        [merged, key.to_string()],
    )?;
    Ok(changed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::credentials::CredentialSource;
    use chrono::Duration;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("lanius-auth-{name}-{}-{id}", std::process::id()))
    }

    #[test]
    fn threshold_is_inclusive_at_six_hundred_seconds() {
        assert!(is_expiring(&Some(Utc::now() + Duration::seconds(600))));
        assert!(!is_expiring(&Some(Utc::now() + Duration::seconds(3600))));
        assert!(is_expiring(&None));
    }

    #[test]
    fn file_write_is_atomic_and_preserves_unknown_fields() {
        let path = temp_path("credentials.json");
        fs::write(&path, r#"{"unknown": "preserved", "accessToken": "old"}"#).unwrap();
        let credentials = Credentials {
            access_token: Some("new-access".into()),
            refresh_token: Some("new-refresh".into()),
            profile_arn: Some("arn:test".into()),
            expires_at: Some(Utc::now()),
            source: Some(CredentialSource::File),
            ..Credentials::default()
        };
        let config = Config {
            kiro_creds_file: Some(path.clone()),
            ..Config::default()
        };
        AuthManager::new(config)
            .unwrap()
            .persist_file(&credentials)
            .unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["unknown"], "preserved");
        assert_eq!(value["accessToken"], "new-access");
        assert!(
            !path
                .with_file_name(format!(
                    ".{}.tmp",
                    path.file_name().unwrap().to_string_lossy()
                ))
                .exists()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn empty_oidc_values_fall_back_to_desktop_auth() {
        let empty = Credentials {
            client_id: Some(String::new()),
            client_secret: Some(String::new()),
            ..Credentials::default()
        };
        let missing_secret = Credentials {
            client_id: Some("client-id".into()),
            client_secret: Some(String::new()),
            ..Credentials::default()
        };
        assert_eq!(AuthType::from_credentials(&empty), AuthType::KiroDesktop);
        assert_eq!(
            AuthType::from_credentials(&missing_secret),
            AuthType::KiroDesktop
        );
    }

    #[test]
    fn sqlite_400_reload_is_limited_to_oidc() {
        assert!(should_reload_oidc_after_http_400(
            AuthType::AwsSsoOidc,
            true,
            reqwest::StatusCode::BAD_REQUEST,
        ));
        assert!(!should_reload_oidc_after_http_400(
            AuthType::KiroDesktop,
            true,
            reqwest::StatusCode::BAD_REQUEST,
        ));
        assert!(!should_reload_oidc_after_http_400(
            AuthType::AwsSsoOidc,
            false,
            reqwest::StatusCode::BAD_REQUEST,
        ));
    }

    #[test]
    fn sqlite_refresh_failure_error_never_exposes_response_credentials() {
        let response_body = r#"{"accessToken":"very-secret-access-token","refreshToken":"very-secret-refresh-token"}"#;
        let error = sqlite_refresh_failed_error(reqwest::StatusCode::BAD_REQUEST);
        assert!(!error.to_string().contains(response_body));
        assert!(!error.user_message().contains(response_body));
        assert!(!error.to_string().contains("very-secret-access-token"));
        assert!(!error.user_message().contains("very-secret-refresh-token"));
        assert_eq!(
            error.user_message(),
            "authentication failed: Token expired and refresh failed. Please run 'kiro-cli login' to refresh your credentials. (HTTP 400)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_credentials_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("private-credentials.json");
        atomic_write_json(&path, &serde_json::json!({"refreshToken": "secret"})).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn atomic_write_removes_temp_file_when_rename_fails() {
        let parent = temp_path("rename-cleanup");
        fs::create_dir_all(&parent).unwrap();
        let destination = parent.join("destination");
        fs::create_dir(&destination).unwrap();
        assert!(
            atomic_write_json(&destination, &serde_json::json!({"refreshToken": "secret"}))
                .is_err()
        );
        let temporary_entries = fs::read_dir(&parent)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".destination.")
            })
            .count();
        assert_eq!(temporary_entries, 0);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn sqlite_write_merges_unknown_fields() {
        let path = temp_path("credentials.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv VALUES (?, ?)",
                [
                    "kirocli:social:token",
                    r#"{"unknown":"kept","refresh_token":"old"}"#,
                ],
            )
            .unwrap();
        let credentials = Credentials {
            access_token: Some("access".into()),
            refresh_token: Some("refresh".into()),
            sso_region: Some("us-east-1".into()),
            sqlite_token_key: Some("kirocli:social:token".into()),
            ..Credentials::default()
        };
        assert!(
            merge_and_save_sqlite_key(
                &connection,
                "kirocli:social:token",
                &credentials,
                "us-east-1"
            )
            .unwrap()
        );
        let saved: Value = serde_json::from_str(
            &connection
                .query_row("SELECT value FROM auth_kv", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["unknown"], "kept");
        assert_eq!(saved["access_token"], "access");
        drop(connection);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn profile_autofetch_request_is_composed_and_parsed_correctly() {
        let hook = ProfileArnAutofetchHook::default();
        assert!(!hook.claim_fetch(false, false), "no token: must not fetch");
        assert!(
            !hook.claim_fetch(true, true),
            "profile present: must not fetch"
        );
        assert!(hook.claim_fetch(true, false), "first eligible call fetches");
        assert!(!hook.claim_fetch(true, false), "must fetch at most once");

        assert_eq!(
            ProfileArnAutofetchHook::profile_url("us-east-1"),
            "https://q.us-east-1.amazonaws.com/"
        );

        let headers =
            ProfileArnAutofetchHook::profile_headers("access-token", "fixture-fingerprint");
        assert_eq!(
            headers[reqwest::header::AUTHORIZATION],
            "Bearer access-token"
        );
        assert_eq!(
            headers["x-amz-target"],
            "AmazonCodeWhispererService.ListAvailableProfiles"
        );
        assert_eq!(
            headers[reqwest::header::USER_AGENT],
            "aws-sdk-js/1.0.27 KiroIDE-0.7.45-fixture-fingerprint"
        );

        assert_eq!(
            ProfileArnAutofetchHook::profile_from_response(
                &serde_json::json!({"profiles":[{"profileArn":"arn:resolved"}]})
            )
            .as_deref(),
            Some("arn:resolved")
        );
        assert_eq!(
            ProfileArnAutofetchHook::profile_from_response(
                &serde_json::json!({"profiles":[{"arn":"arn:alt"}]})
            )
            .as_deref(),
            Some("arn:alt")
        );
        assert!(
            ProfileArnAutofetchHook::profile_from_response(&serde_json::json!({"profiles":[]}))
                .is_none()
        );
    }

    #[tokio::test]
    async fn host_hooks_fall_back_for_profileless_chat_and_keep_control_plane_separate() {
        let profileless = AuthManager::new(Config::default())
            .unwrap_or_else(|error| panic!("auth must initialize: {error}"));
        assert_eq!(
            profileless.api_host().await,
            "https://q.us-east-1.amazonaws.com"
        );
        assert_eq!(
            profileless.control_plane_host().await,
            "https://q.us-east-1.amazonaws.com"
        );
        let paid = AuthManager::new(Config {
            profile_arn: Some("arn:paid".into()),
            ..Config::default()
        })
        .unwrap_or_else(|error| panic!("auth must initialize: {error}"));
        assert_eq!(paid.api_host().await, "https://runtime.us-east-1.kiro.dev");
        assert_eq!(
            paid.control_plane_host().await,
            "https://q.us-east-1.amazonaws.com"
        );
    }
}
