//! Performs the network call that exchanges a refresh token for a new access token.
//!
//! This is the "how do we get a new one" half of [`super`] (the auth module), invoked
//! exclusively by [`super::AuthManager::refresh_locked`] while that caller holds the
//! credential state lock — so at most one refresh request is ever in flight for a given
//! [`super::AuthManager`], and this module itself does not need to worry about concurrent
//! refresh races.
//!
//! There are two supported flows, selected by [`super::AuthType`]:
//! - [`super::AuthType::KiroDesktop`]: POSTs `{"refreshToken": ...}` to the Kiro desktop
//!   refresh endpoint.
//! - [`super::AuthType::AwsSsoOidc`]: POSTs the AWS SSO OIDC `refresh_token` grant body
//!   (client id/secret plus refresh token) to the OIDC token endpoint.
//!
//! Security note: every error path here is careful to surface only the HTTP status code
//! or a fixed, non-parameterized message — never the raw response body — because that
//! body may legitimately contain a freshly issued (or about-to-be-superseded) access or
//! refresh token. See [`http_status_error`] and [`RefreshError::HttpStatus`].

use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::error::{GatewayError, Result};
use crate::utils::{API_SSO_OIDC, KIRO_CLI_REFRESH_USER_AGENT, aws_sdk_user_agents};

use super::AuthType;
use super::credentials::Credentials;

/// The refresh-endpoint URLs for the current region, built by
/// [`super::AuthManager::endpoints`] from region-specific URL templates.
#[derive(Debug, Clone)]
pub(crate) struct RefreshEndpoints {
    pub desktop_url: String,
    pub oidc_url: String,
}

/// The parsed result of a successful refresh call: a new access token, and optionally an
/// updated refresh token / profile ARN (only present if the upstream response actually
/// rotated them) plus the computed expiry time.
#[derive(Debug, Clone)]
pub(crate) struct RefreshOutcome {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub profile_arn: Option<String>,
    pub expires_at: DateTime<Utc>,
}

/// A refresh failure, distinguishing an HTTP-level failure (whose status code the caller
/// may want to react to, e.g. to trigger a SQLite reload-and-retry) from any other
/// [`GatewayError`] (network failure, malformed response, missing required credential
/// field, etc.).
#[derive(Debug)]
pub(crate) enum RefreshError {
    HttpStatus(StatusCode),
    Gateway(GatewayError),
}

impl From<GatewayError> for RefreshError {
    fn from(error: GatewayError) -> Self {
        Self::Gateway(error)
    }
}

/// Builds the request body for the Kiro desktop refresh endpoint. Intentionally minimal:
/// only the refresh token, no client credentials or scope.
pub(crate) fn desktop_body(refresh_token: &str) -> Value {
    json!({"refreshToken": refresh_token})
}

/// Builds the request body for the AWS SSO OIDC `refresh_token` grant. Deliberately omits
/// any `scope`/`scopes` field — the OIDC endpoint used here does not accept one, and
/// including it has caused rejected requests in the past (see the accompanying test
/// `oidc_body_is_camel_case_and_never_sends_scope`).
pub(crate) fn oidc_body(client_id: &str, client_secret: &str, refresh_token: &str) -> Value {
    json!({
        "grantType": "refresh_token",
        "clientId": client_id,
        "clientSecret": client_secret,
        "refreshToken": refresh_token,
    })
}

/// Headers for the Kiro social-login refresh endpoint, as sent by the Kiro CLI.
fn desktop_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Content-Type", "application/json".to_string()),
        ("Accept", "*/*".to_string()),
        ("User-Agent", KIRO_CLI_REFRESH_USER_AGENT.to_string()),
    ]
}

/// Headers for the AWS SSO OIDC token endpoint, as sent by the Kiro CLI's
/// SSO OIDC SDK client.
fn oidc_headers() -> Vec<(&'static str, String)> {
    let (user_agent, amz_user_agent) = aws_sdk_user_agents(API_SSO_OIDC);
    vec![
        ("Content-Type", "application/json".to_string()),
        ("User-Agent", user_agent),
        ("x-amz-user-agent", amz_user_agent),
    ]
}

/// Performs one refresh HTTP request for the given `auth_type`, using the credentials'
/// current refresh token (and, for OIDC, client id/secret). Always performs exactly one
/// network request when it reaches the point of sending; returns before sending if a
/// required credential field is missing/empty.
///
/// Expiry-time computation:
/// - The upstream `expiresIn` (seconds) defaults to 3600 if absent, and 60 seconds are
///   subtracted before converting to an absolute timestamp — a small safety margin so we
///   proactively refresh slightly before the token would actually expire server-side.
/// - For [`AuthType::KiroDesktop`], the "now" base timestamp has its sub-second component
///   zeroed (`with_nanosecond(0)`) before the margin is applied; AWS SSO OIDC does not get
///   this rounding. The practical effect is sub-second either way.
pub(crate) async fn refresh(
    auth_type: AuthType,
    credentials: &Credentials,
    endpoints: &RefreshEndpoints,
) -> std::result::Result<RefreshOutcome, RefreshError> {
    let refresh_token = required(&credentials.refresh_token, "Refresh token is not set")?;
    let (url, headers, payload, profile_arn) = match auth_type {
        AuthType::KiroDesktop => (
            &endpoints.desktop_url,
            desktop_headers(),
            desktop_body(refresh_token),
            true,
        ),
        AuthType::AwsSsoOidc => {
            let client_id = required(
                &credentials.client_id,
                "Client ID is not set (required for AWS SSO OIDC)",
            )?;
            let client_secret = required(
                &credentials.client_secret,
                "Client secret is not set (required for AWS SSO OIDC)",
            )?;
            (
                &endpoints.oidc_url,
                oidc_headers(),
                oidc_body(client_id, client_secret, refresh_token),
                false,
            )
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| GatewayError::Auth(format!("failed to create refresh client: {error}")))?;
    let mut request = client.post(url).json(&payload);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| GatewayError::Auth(format!("token refresh request failed: {error}")))?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        GatewayError::Auth(format!("failed to read token refresh response: {error}"))
    })?;
    if !status.is_success() {
        // `body` is deliberately not forwarded into the error: it may contain a live
        // token. See `http_status_error`.
        return Err(http_status_error(status, &body));
    }
    let data: Value = serde_json::from_str(&body).map_err(|error| {
        GatewayError::Auth(format!("invalid token refresh JSON response: {error}"))
    })?;
    let access_token = access_token_from_response(&data, profile_arn)?;
    let expires_in = data
        .get("expiresIn")
        .and_then(Value::as_f64)
        .unwrap_or(3600.0);
    let now = Utc::now();
    let base = if matches!(auth_type, AuthType::KiroDesktop) {
        now.with_nanosecond(0).unwrap_or(now)
    } else {
        now
    };
    // Subtract 60s from the reported lifetime as a refresh-ahead safety margin.
    let expires_at = base + chrono::Duration::milliseconds(((expires_in - 60.0) * 1000.0) as i64);
    Ok(RefreshOutcome {
        access_token,
        refresh_token: data
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(ToOwned::to_owned),
        // Only the desktop flow (`profile_arn == true`) is expected to return a
        // `profileArn` field; for OIDC this is always `None` regardless of response
        // content.
        profile_arn: profile_arn
            .then(|| {
                data.get("profileArn")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .flatten(),
        expires_at,
    })
}

/// Returns `value` as `&str` if present and non-empty, otherwise a
/// [`RefreshError::Gateway`] carrying `message`. Used to validate required credential
/// fields before making a network request.
fn required<'a>(
    value: &'a Option<String>,
    message: &str,
) -> std::result::Result<&'a str, RefreshError> {
    value
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::Auth(message.to_string()).into())
}

/// Constructs an HTTP-status refresh error. `response_body` is accepted only so call
/// sites are forced to acknowledge it exists, but it is deliberately *not* included in the
/// returned error — the upstream error body can contain sensitive token material, so only
/// the status code is preserved for diagnostics.
fn http_status_error(status: StatusCode, _response_body: &str) -> RefreshError {
    RefreshError::HttpStatus(status)
}

/// Extracts `accessToken` from a successful refresh response, producing a flow-specific
/// error message (desktop vs. AWS SSO OIDC) if it is missing or empty. Never echoes any
/// other field of `data` (which may include client secrets or tokens) into the error.
fn access_token_from_response(data: &Value, profile_arn: bool) -> Result<String> {
    data.get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            let message = if profile_arn {
                "Response does not contain accessToken"
            } else {
                "AWS SSO OIDC response does not contain accessToken"
            };
            GatewayError::Auth(message.to_string())
        })
}

/// Converts a [`RefreshError`] into the crate's standard [`Result`], for call sites that
/// have already handled the `Ok` case and just need to propagate a failure. Always returns
/// `Err`.
pub(crate) fn into_result(error: RefreshError) -> Result<()> {
    match error {
        RefreshError::HttpStatus(status) => Err(GatewayError::Auth(format!(
            "token refresh failed with HTTP {}",
            status.as_u16()
        ))),
        RefreshError::Gateway(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_body_has_only_camel_case_refresh_token() {
        assert_eq!(desktop_body("refresh"), json!({"refreshToken": "refresh"}));
    }

    #[test]
    fn refresh_http_error_never_exposes_response_credentials() {
        let response_body = r#"{"accessToken":"very-secret-access-token","refreshToken":"very-secret-refresh-token"}"#;
        let error =
            into_result(http_status_error(StatusCode::BAD_REQUEST, response_body)).unwrap_err();
        assert!(!error.to_string().contains(response_body));
        assert!(!error.user_message().contains(response_body));
        assert!(!error.to_string().contains("very-secret-access-token"));
        assert!(!error.user_message().contains("very-secret-refresh-token"));
        assert_eq!(
            error.user_message(),
            "authentication failed: token refresh failed with HTTP 400"
        );
    }

    #[test]
    fn missing_access_token_error_never_echoes_sensitive_response_fields() {
        let secret = "very-secret-client-secret";
        let data = json!({"refreshToken": "refresh-token", "clientSecret": secret});
        let error = access_token_from_response(&data, false).unwrap_err();
        assert!(!error.to_string().contains(secret));
        assert!(!error.user_message().contains(secret));
        assert_eq!(
            error.user_message(),
            "authentication failed: AWS SSO OIDC response does not contain accessToken"
        );
    }

    #[test]
    fn refresh_headers_identify_as_kiro_cli() {
        let desktop = desktop_headers();
        assert!(desktop.contains(&("User-Agent", "Kiro-CLI".to_string())));
        let oidc = oidc_headers();
        let amz = &oidc
            .iter()
            .find(|(name, _)| *name == "x-amz-user-agent")
            .unwrap()
            .1;
        assert!(amz.contains("api/ssooidc/") && amz.ends_with("app/AmazonQ-For-CLI"));
        for (_, value) in desktop.iter().chain(&oidc) {
            assert!(!value.contains("KiroIDE"));
        }
    }

    #[test]
    fn oidc_body_is_camel_case_and_never_sends_scope() {
        let body = oidc_body("id", "secret", "refresh");
        assert_eq!(
            body,
            json!({
                "grantType": "refresh_token",
                "clientId": "id",
                "clientSecret": "secret",
                "refreshToken": "refresh",
            })
        );
        let object = body.as_object().unwrap();
        assert!(!object.contains_key("scope"));
        assert!(!object.contains_key("scopes"));
    }
}
