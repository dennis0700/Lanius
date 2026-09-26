//! Loading raw authentication credentials from environment variables, a JSON credentials
//! file, or a `kiro-cli` SQLite database.
//!
//! This module is the "where do the tokens come from" half of [`super`] (the auth module);
//! [`super::refresh`] is the "how do we get a new one" half. [`Credentials::load`] is the
//! single entry point used by [`super::AuthManager::new`], and [`Credentials::merge_sqlite`]
//! / [`Credentials::merge_file`] are also called later, standalone, when the manager needs
//! to reload credentials mid-run (e.g. after a SQLite-backed refresh-token rotation).
//!
//! Priority rules, in order of precedence when multiple sources are present:
//! 1. A `refreshToken`/profile ARN supplied directly via environment/config
//!    ([`Config::refresh_token`], [`Config::profile_arn`]) seeds the initial value but can
//!    still be overwritten by a SQLite or file value if one is configured and found.
//! 2. If a `kiro-cli` SQLite database path is configured, it is consulted
//!    ([`Credentials::merge_sqlite`]) and takes priority over a JSON credentials file.
//! 3. Otherwise, if a JSON credentials file path is configured, it is consulted
//!    ([`Credentials::merge_file`]).
//!
//! Within SQLite, [`SQLITE_TOKEN_KEYS`] defines the fallback order for which `auth_kv` row
//! holds the live token (social login takes priority over OIDC device-registration-based
//! logins), and the enterprise device-registration fallback in
//! [`Credentials::merge_enterprise_registration`] reads a separate `~/.aws/sso/cache/*.json`
//! file when a `clientIdHash` is present in a JSON credentials file.
//!
//! Security note: this module exists specifically to read tokens and client secrets. It
//! must never log the *values* it reads (only paths/keys on error), and callers elsewhere
//! (see [`super`]'s error constructors) must take care not to echo secrets read from here
//! back into user-facing error messages.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::error::{GatewayError, Result};

/// `auth_kv` keys, in priority order, that may hold the live access/refresh token pair in
/// a `kiro-cli` SQLite database. The first key with a row wins (see
/// [`Credentials::merge_sqlite`]).
pub(crate) const SQLITE_TOKEN_KEYS: [&str; 3] = [
    "kirocli:social:token",
    "kirocli:odic:token",
    "codewhisperer:odic:token",
];
/// `auth_kv` keys that may hold the OIDC device-registration (client id/secret) used for
/// the AWS SSO OIDC refresh flow. Only the first matching key is used.
pub(crate) const SQLITE_REGISTRATION_KEYS: [&str; 2] = [
    "kirocli:odic:device-registration",
    "codewhisperer:odic:device-registration",
];

/// Records which backing store a [`Credentials`] value was most recently merged from, for
/// diagnostics/testing purposes only — it does not itself change how credentials are used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialSource {
    Environment,
    File,
    Sqlite,
}

/// The full set of credential fields the gateway may need, gathered from whichever
/// backing store(s) are configured. All fields are optional because not every source
/// populates every field (e.g. a plain refresh-token-only setup has no `client_id`).
///
/// This type intentionally derives only `Debug`/`Clone`/`Default` and not `Serialize` —
/// it is never meant to be dumped wholesale into a response or log line.
#[derive(Debug, Clone, Default)]
pub(crate) struct Credentials {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub profile_arn: Option<String>,
    pub sso_region: Option<String>,
    pub detected_api_region: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Option<Value>,
    /// The specific SQLite `auth_kv` key these credentials were loaded from, if any —
    /// remembered so a later write-back (see `super::AuthManager::persist_sqlite`) updates
    /// the same row rather than guessing.
    pub sqlite_token_key: Option<String>,
    pub source: Option<CredentialSource>,
}

impl Credentials {
    /// Loads credentials starting from any environment-supplied refresh token/profile ARN
    /// in `config`, then merges in a SQLite database (if configured) or else a JSON
    /// credentials file (if configured), per the module-level priority rules.
    ///
    /// Performs local file/database I/O only; never makes a network request.
    pub(crate) fn load(config: &Config) -> Result<Self> {
        let mut credentials = Self {
            refresh_token: config.refresh_token.clone(),
            profile_arn: config.profile_arn.clone(),
            source: config
                .refresh_token
                .as_ref()
                .filter(|token| !token.is_empty())
                .map(|_| CredentialSource::Environment),
            ..Self::default()
        };

        if let Some(path) = &config.kiro_cli_db_file {
            credentials.merge_sqlite(path)?;
        } else if let Some(path) = &config.kiro_creds_file {
            credentials.merge_file(path)?;
        }
        Ok(credentials)
    }

    /// Merges fields from a JSON credentials file into `self`, overwriting any field the
    /// file provides a value for. Missing files, unreadable files, malformed JSON, or a
    /// non-object root are all treated as soft failures — logged at `warn`/`error` and
    /// otherwise ignored, so a broken credentials file degrades to "no credentials from
    /// this source" rather than crashing the gateway.
    pub(crate) fn merge_file(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            tracing::warn!(path = %path.display(), "credentials file not found");
            return Ok(());
        }
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::error!(path = %path.display(), %error, "failed to read credentials file");
                return Ok(());
            }
        };
        let data: Value = match serde_json::from_str(&raw) {
            Ok(data) => data,
            Err(error) => {
                tracing::error!(path = %path.display(), %error, "failed to parse credentials file");
                return Ok(());
            }
        };
        let object = match data.as_object() {
            Some(object) => object,
            None => {
                tracing::error!(path = %path.display(), "credentials file root is not an object");
                return Ok(());
            }
        };

        merge_string(object, "accessToken", &mut self.access_token);
        merge_string(object, "refreshToken", &mut self.refresh_token);
        merge_string(object, "profileArn", &mut self.profile_arn);
        if let Some(region) = string_at(object, "region") {
            self.sso_region = Some(region.to_string());
            self.detected_api_region = Some(region.to_string());
        }
        merge_string(object, "clientId", &mut self.client_id);
        merge_string(object, "clientSecret", &mut self.client_secret);
        if let Some(client_id_hash) = string_at(object, "clientIdHash") {
            self.merge_enterprise_registration(client_id_hash);
        }
        if let Some(value) = string_at(object, "expiresAt") {
            self.expires_at = parse_iso8601(value);
        }
        self.source = Some(CredentialSource::File);
        Ok(())
    }

    /// Merges fields from a `kiro-cli` SQLite database into `self`. Like
    /// [`Credentials::merge_file`], failures to open or read the database are logged and
    /// swallowed rather than propagated.
    ///
    /// Three independent lookups happen, each best-effort and each falling back silently:
    /// 1. The token/refresh-token pair, tried against [`SQLITE_TOKEN_KEYS`] in priority
    ///    order — the first key with a row present wins, and that key is remembered in
    ///    `sqlite_token_key` for later write-back.
    /// 2. The OIDC client id/secret device registration, tried against
    ///    [`SQLITE_REGISTRATION_KEYS`] in order; the loop `break`s on the first key that has
    ///    *any* row (even if its JSON turned out to be malformed), matching the token
    ///    lookup's "first candidate wins" semantics rather than trying every key on error.
    /// 3. The active Kiro profile ARN and, from its ARN, a detected API region — read from
    ///    the separate `state` table's `api.codewhisperer.profile` row, and only used to
    ///    fill in `profile_arn`/`detected_api_region` if they are not already set from the
    ///    token lookup above.
    pub(crate) fn merge_sqlite(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            tracing::warn!(path = %path.display(), "SQLite credential database not found");
            return Ok(());
        }
        let connection = match open_read_only(path) {
            Ok(connection) => connection,
            Err(error) => {
                tracing::error!(path = %path.display(), %error, "failed to open SQLite credential database");
                return Ok(());
            }
        };

        let token_row = SQLITE_TOKEN_KEYS.iter().find_map(|key| {
            connection
                .query_row("SELECT value FROM auth_kv WHERE key = ?", [key], |row| {
                    row.get::<_, String>(0)
                })
                .ok()
                .map(|value| (*key, value))
        });
        if let Some((key, raw)) = token_row {
            let data: Value = match serde_json::from_str(&raw) {
                Ok(data) => data,
                Err(error) => {
                    tracing::error!(%error, key, "invalid token JSON in SQLite credentials");
                    return Ok(());
                }
            };
            if let Some(object) = data.as_object() {
                merge_string(object, "access_token", &mut self.access_token);
                merge_string(object, "refresh_token", &mut self.refresh_token);
                merge_string(object, "profile_arn", &mut self.profile_arn);
                if let Some(region) = string_at(object, "region") {
                    self.sso_region = Some(region.to_string());
                }
                self.scopes = object.get("scopes").cloned();
                if let Some(value) = string_at(object, "expires_at") {
                    self.expires_at = parse_iso8601(value);
                }
                self.sqlite_token_key = Some(key.to_string());
                self.source = Some(CredentialSource::Sqlite);
            }
        }

        for key in SQLITE_REGISTRATION_KEYS {
            let registration = connection
                .query_row("SELECT value FROM auth_kv WHERE key = ?", [key], |row| {
                    row.get::<_, String>(0)
                })
                .ok();
            if let Some(raw) = registration {
                if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&raw) {
                    merge_string(&object, "client_id", &mut self.client_id);
                    merge_string(&object, "client_secret", &mut self.client_secret);
                    if self.sso_region.is_none() {
                        if let Some(region) = string_at(&object, "region") {
                            self.sso_region = Some(region.to_string());
                        }
                    }
                }
                // Only the first registration key that has a row is consulted, whether or
                // not its JSON turned out to be usable — the remaining keys are treated as
                // alternates for a different device-registration scheme, not additional
                // fallbacks for a corrupted row under the same scheme.
                break;
            }
        }

        if let Ok(raw) = connection.query_row(
            "SELECT value FROM state WHERE key = 'api.codewhisperer.profile'",
            [],
            |row| row.get::<_, String>(0),
        ) {
            if let Ok(Value::Object(profile)) = serde_json::from_str::<Value>(&raw) {
                if let Some(arn) = string_at(&profile, "arn") {
                    if self.profile_arn.is_none() {
                        self.profile_arn = Some(arn.to_string());
                    }
                    if let Some(region) = api_region_from_arn(arn) {
                        self.detected_api_region = Some(region);
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads the AWS SSO OIDC client id/secret device registration for `client_id_hash`
    /// from `~/.aws/sso/cache/<hash>.json`, used when a JSON credentials file references an
    /// Enterprise SSO registration by hash rather than embedding the client secret
    /// directly. Missing home directory, missing/unreadable file, or invalid JSON are all
    /// logged and ignored rather than propagated as errors.
    fn merge_enterprise_registration(&mut self, client_id_hash: &str) {
        let Some(home) = dirs::home_dir() else {
            tracing::warn!("cannot locate home directory for Enterprise device registration");
            return;
        };
        let path = home
            .join(".aws")
            .join("sso")
            .join("cache")
            .join(format!("{client_id_hash}.json"));
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Enterprise device registration unavailable");
                return;
            }
        };
        match serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|value| value.as_object().cloned())
        {
            Some(object) => {
                merge_string(&object, "clientId", &mut self.client_id);
                merge_string(&object, "clientSecret", &mut self.client_secret);
            }
            None => {
                tracing::warn!(path = %path.display(), "invalid Enterprise device registration JSON")
            }
        }
    }

    /// Returns the configured JSON credentials file path, if any — used by
    /// `super::AuthManager` to know where to write refreshed credentials back to.
    pub(crate) fn source_path(config: &Config) -> Option<PathBuf> {
        config.kiro_creds_file.clone()
    }
}

/// Opens the SQLite database at `path` strictly read-only via a `file:` URI with
/// `mode=ro`, so that reading credentials can never accidentally create or modify the
/// on-disk database (write-back, when needed, uses a separate read-write connection in
/// `super::AuthManager::persist_sqlite`).
pub(crate) fn open_read_only(path: &Path) -> std::result::Result<Connection, rusqlite::Error> {
    let uri = format!("file:{}?mode=ro", path.display());
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

/// Parses an ISO-8601/RFC-3339 timestamp, first via strict RFC-3339 parsing (handles a
/// trailing `Z` and fractional seconds), then falling back to a naive
/// `%Y-%m-%dT%H:%M:%S%.f` format (assumed UTC) for timestamps that omit a timezone
/// designator entirely. Returns `None` if neither format matches.
pub(crate) fn parse_iso8601(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|time| DateTime::<Utc>::from_naive_utc_and_offset(time, Utc))
        })
}

/// Extracts and validates the AWS region segment (the 4th colon-delimited field) from a
/// profile ARN, e.g. `arn:aws:codewhisperer:eu-central-1:...` -> `Some("eu-central-1")`.
/// The region must match the conventional `<letters>-<letters>-<digits>` shape (lowercase
/// letters only, exactly three dash-separated parts) or `None` is returned — this
/// intentionally mirrors a stricter upstream regex check so we don't treat an unexpected
/// or malformed ARN segment as a valid region.
pub(crate) fn api_region_from_arn(arn: &str) -> Option<String> {
    let region = arn.split(':').nth(3)?;
    let mut parts = region.split('-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), Some(number), None)
            if !a.is_empty()
                && !b.is_empty()
                && a.chars().all(|character| character.is_ascii_lowercase())
                && b.chars().all(|character| character.is_ascii_lowercase())
                && number.chars().all(|character| character.is_ascii_digit()) =>
        {
            Some(region.to_string())
        }
        _ => None,
    }
}

fn string_at<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    object.get(field).and_then(Value::as_str)
}

fn merge_string(object: &Map<String, Value>, field: &str, target: &mut Option<String>) {
    if let Some(value) = string_at(object, field) {
        *target = Some(value.to_string());
    }
}

/// Wraps a low-level `rusqlite` error as a [`GatewayError::Sqlite`], without embedding any
/// query parameters (which could include credential values) beyond the driver's own error
/// message.
pub(crate) fn sqlite_error(error: rusqlite::Error) -> GatewayError {
    GatewayError::Sqlite(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "lanius-credentials-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    // Verifies SQLite key priority (social > oidc > codewhisperer-oidc), the enterprise
    // device-registration merge, and ARN-derived region detection, using placeholder
    // (non-real) token values.
    #[test]
    fn sqlite_load_uses_real_schema_priority_and_odic_keys() {
        let path = temp_db();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT); CREATE TABLE state (key TEXT PRIMARY KEY, value TEXT);").unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv VALUES (?, ?)",
                [
                    "codewhisperer:odic:token",
                    r#"{"access_token":"legacy","refresh_token":"legacy-refresh"}"#,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv VALUES (?, ?)",
                [
                    "kirocli:odic:token",
                    r#"{"access_token":"oidc","refresh_token":"oidc-refresh"}"#,
                ],
            )
            .unwrap();
        connection.execute("INSERT INTO auth_kv VALUES (?, ?)", ["kirocli:social:token", r#"{"access_token":"social","refresh_token":"social-refresh","region":"eu-west-1","expires_at":"2099-01-01T00:00:00.123456789Z"}"#]).unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv VALUES (?, ?)",
                [
                    "kirocli:odic:device-registration",
                    r#"{"client_id":"client","client_secret":"secret"}"#,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO state VALUES (?, ?)",
                [
                    "api.codewhisperer.profile",
                    r#"{"arn":"arn:aws:codewhisperer:eu-central-1:123:profile/p"}"#,
                ],
            )
            .unwrap();
        drop(connection);

        let config = Config {
            kiro_cli_db_file: Some(path.clone()),
            ..Config::default()
        };
        let credentials = Credentials::load(&config).unwrap();
        assert_eq!(credentials.access_token.as_deref(), Some("social"));
        assert_eq!(
            credentials.sqlite_token_key.as_deref(),
            Some("kirocli:social:token")
        );
        assert_eq!(credentials.client_id.as_deref(), Some("client"));
        assert_eq!(
            credentials.detected_api_region.as_deref(),
            Some("eu-central-1")
        );
        assert_eq!(
            credentials.expires_at.unwrap().timestamp_subsec_nanos(),
            123_456_789
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sqlite_uri_is_read_only() {
        let path = temp_db();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        drop(connection);
        let read_only = open_read_only(&path).unwrap();
        assert!(
            read_only
                .execute("CREATE TABLE should_fail (x INTEGER)", [])
                .is_err()
        );
        drop(read_only);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn iso_parser_accepts_z_and_nanoseconds() {
        assert_eq!(
            parse_iso8601("2099-01-01T00:00:00.123456789Z")
                .unwrap()
                .timestamp_subsec_nanos(),
            123_456_789
        );
        assert!(parse_iso8601("not-a-date").is_none());
    }

    #[test]
    fn arn_region_validation_is_strict() {
        assert_eq!(
            api_region_from_arn("arn:aws:codewhisperer:eu-central-1:1:p").as_deref(),
            Some("eu-central-1")
        );
        assert_eq!(
            api_region_from_arn("arn:aws:codewhisperer:EU-central-1:1:p"),
            None
        );
    }
}
