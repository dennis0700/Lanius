//! Fetches and normalizes Kiro usage/quota data for the configured account.
//!
//! [`fetch_usage`] hits Kiro's control-plane `GetUsageLimits` operation and
//! is the single source of truth for both the gateway's `GET /usage` /
//! `GET /account` routes (see [`crate::server`]) and the `lanius usage` /
//! `lanius account` CLI subcommands, so the two surfaces can never drift.
//! [`account_summary`] reshapes the raw payload into a flatter, UI-friendly
//! shape.

use chrono::TimeZone;
use serde_json::{Value, json};

use crate::auth::AuthManager;
use crate::error::{GatewayError, Result, enhance_kiro_error};
use crate::upstream::KiroHttpClient;
use crate::utils::{API_RUNTIME, KIRO_ORIGIN, with_query};

/// Fetches raw usage-limits data from Kiro's control plane using the
/// configured account's auth.
///
/// # Examples
///
/// ```no_run
/// use lanius_core::account::fetch_usage;
/// use lanius_core::auth::AuthManager;
/// use lanius_core::upstream::KiroHttpClient;
/// use lanius_core::Config;
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = Config::default();
/// let auth = Arc::new(AuthManager::new(config.clone())?);
/// let client = KiroHttpClient::new(auth.clone(), &config)?;
/// let usage = fetch_usage(&auth, &client).await?;
/// # let _ = usage;
/// # Ok(())
/// # }
/// ```
pub async fn fetch_usage(auth: &AuthManager, http_client: &KiroHttpClient) -> Result<Value> {
    let token = auth.access_token_and_autofetch().await?;
    let mut payload = json!({"origin":KIRO_ORIGIN,"isEmailRequired":true});
    // Mirrors the Kiro CLI: the inputs go both in the query string and the
    // body. The control plane requires the profile ARN for CLI-identified
    // requests.
    let profile_arn = auth.profile_arn().await.filter(|arn| !arn.is_empty());
    let mut query = Vec::with_capacity(2);
    if let Some(arn) = &profile_arn {
        query.push(("profileArn", arn.as_str()));
        payload["profileArn"] = Value::String(arn.clone());
    }
    query.push(("origin", KIRO_ORIGIN));
    let body = serde_json::to_vec(&payload)?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in auth.headers_for(&token, API_RUNTIME) {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    headers.insert(
        "x-amz-target",
        reqwest::header::HeaderValue::from_static("AmazonCodeWhispererService.GetUsageLimits"),
    );
    let url = with_query(&format!("{}/", auth.control_plane_host().await), &query);
    let mut request = http_client.client().post(url).body(body);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if status != reqwest::StatusCode::OK {
        return Err(usage_error_from_status(status, &bytes));
    }
    serde_json::from_slice(&bytes).map_err(|_| GatewayError::Upstream {
        status: 502,
        info: Box::new(enhance_kiro_error(&json!({
            "message": "Failed to fetch usage: invalid JSON response"
        }))),
    })
}

// Builds a generic error for a failed control-plane usage request without
// leaking the upstream response body (which may contain sensitive account
// details) into the error returned to callers; only the status and body
// length are logged.
fn usage_error_from_status(status: reqwest::StatusCode, bytes: &[u8]) -> GatewayError {
    tracing::debug!(
        upstream_status = %status,
        upstream_body_bytes = bytes.len(),
        upstream_body = "[redacted]",
        "control-plane usage request returned non-success status"
    );
    GatewayError::Upstream {
        status: status.as_u16(),
        info: Box::new(enhance_kiro_error(&json!({
            "message": "Failed to fetch usage from upstream"
        }))),
    }
}

/// Reshapes Kiro's raw usage-limits JSON into a flatter, UI-friendly account
/// summary: combines free-trial and standard quota/usage, sums any active
/// bonus allotments, and preserves "falsy-is-absent" semantics (0 quota/usage
/// is treated the same as missing).
///
/// # Examples
///
/// ```
/// use lanius_core::account::account_summary;
/// use serde_json::json;
///
/// let summary = account_summary(&json!({
///     "userInfo": {"email": "a@b"},
///     "subscriptionInfo": {"type": "Pro"},
///     "usageBreakdownList": [{"usageLimit": 100, "currentUsage": 10}],
/// }));
/// assert_eq!(summary["totalQuota"], 100.0);
/// ```
pub fn account_summary(usage: &Value) -> Value {
    let user = usage.get("userInfo").and_then(Value::as_object);
    let subscription = usage.get("subscriptionInfo").and_then(Value::as_object);
    let breakdown = usage
        .get("usageBreakdownList")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(Value::as_object);
    let empty = serde_json::Map::new();
    let user = user.unwrap_or(&empty);
    let subscription = subscription.unwrap_or(&empty);
    let breakdown = breakdown.unwrap_or(&empty);
    let trial = breakdown
        .get("freeTrialInfo")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let is_trial = trial.get("freeTrialStatus").and_then(Value::as_str) == Some("ACTIVE");
    let trial_quota = precision_or_regular(trial, "usageLimitWithPrecision", "usageLimit");
    let trial_usage = precision_or_regular(trial, "currentUsageWithPrecision", "currentUsage");
    let free_quota = precision_or_regular(breakdown, "usageLimitWithPrecision", "usageLimit");
    let free_usage = precision_or_regular(breakdown, "currentUsageWithPrecision", "currentUsage");
    let bonuses: Vec<&serde_json::Map<String, Value>> = breakdown
        .get("bonuses")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_object)
                .filter(|bonus| bonus.get("status").and_then(Value::as_str) == Some("ACTIVE"))
                .collect()
        })
        .unwrap_or_default();
    let bonus_quota = (!bonuses.is_empty()).then(|| {
        bonuses
            .iter()
            .map(|bonus| precision_or_regular(bonus, "usageLimitWithPrecision", "usageLimit"))
            .sum::<f64>()
    });
    let bonus_usage = (!bonuses.is_empty()).then(|| {
        bonuses
            .iter()
            .map(|bonus| precision_or_regular(bonus, "currentUsageWithPrecision", "currentUsage"))
            .sum::<f64>()
    });
    let total_quota = trial_quota + free_quota;
    let total_usage = trial_usage + free_usage;
    let trial_expiry = trial
        .get("freeTrialExpiry")
        .and_then(Value::as_i64)
        .and_then(|seconds| chrono::Local.timestamp_opt(seconds, 0).single())
        .map(|time| time.format("%Y-%m-%d").to_string());
    let account_status = subscription
        .get("status")
        .cloned()
        .unwrap_or_else(|| Value::String(if is_trial { "Trial" } else { "Active" }.into()));
    json!({
        "accountName": user.get("email").cloned().unwrap_or_else(|| Value::String("User".into())), "email": user.get("email").cloned().unwrap_or(Value::Null), "provider": user.get("provider").cloned().unwrap_or(Value::Null),
        "planType": subscription.get("type").cloned().unwrap_or_else(|| Value::String("Free".into())), "subscriptionTitle": subscription.get("subscriptionTitle").cloned().unwrap_or(Value::Null), "isTrial": is_trial,
        "trialExpiryDate": trial_expiry, "subscriptionExpiryDate": subscription.get("expiryDate").cloned().or_else(|| subscription.get("subscriptionExpiryDate").cloned()).unwrap_or(Value::Null),
        "totalQuota": total_quota, "currentUsage": total_usage, "remainingQuota": (total_quota != 0.0 && total_usage != 0.0).then_some(total_quota-total_usage), "usagePercentage": if total_quota != 0.0 && total_usage != 0.0 { ((total_usage / total_quota * 100.0) * 100.0).round() / 100.0 } else { 0.0 },
        "trialQuota": trial_quota, "trialUsage": trial_usage, "freeQuota": free_quota, "freeUsage": free_usage,
        "bonusQuota": bonus_quota, "bonusUsage": bonus_usage, "bonusRemaining": bonus_quota.zip(bonus_usage).and_then(|(quota, used)| (quota != 0.0 && used != 0.0).then_some(quota-used)),
        "accountStatus": account_status, "resetDate": breakdown.get("resetDate").cloned().unwrap_or(Value::Null), "overageEnabled": breakdown.get("overageEnabled").cloned().unwrap_or(Value::Bool(false)),
    })
}

// Prefers a "with precision" numeric field over its rounded counterpart,
// falling back to the rounded field when the precise one is absent or zero
// (Kiro sometimes omits precision fields, represented as 0.0 here).
fn precision_or_regular(
    values: &serde_json::Map<String, Value>,
    precision: &str,
    regular: &str,
) -> f64 {
    values
        .get(precision)
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .or_else(|| values.get(regular).and_then(Value::as_f64))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_summary_keeps_falsy_is_absent_semantics_and_all_fields() {
        let at = chrono::Local
            .with_ymd_and_hms(2026, 1, 2, 0, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("fixed local time must exist"));
        let value = account_summary(
            &json!({"userInfo":{"email":"a@b","provider":"x"},"subscriptionInfo":{"type":"Pro"},"usageBreakdownList":[{"freeTrialInfo":{"freeTrialStatus":"ACTIVE","freeTrialExpiry":at.timestamp(),"usageLimit":5,"currentUsage":2},"usageLimit":0,"currentUsage":0,"bonuses":[{"status":"ACTIVE","usageLimit":3,"currentUsage":1}]}]}),
        );
        assert_eq!(value.as_object().map(serde_json::Map::len), Some(22));
        assert_eq!(value["remainingQuota"], 3.0);
        assert_eq!(value["bonusRemaining"], 2.0);
    }

    #[test]
    fn control_plane_error_body_is_not_leaked_into_the_error() {
        let secret = b"Authorization: Bearer upstream-secret-token";
        let error = usage_error_from_status(reqwest::StatusCode::BAD_GATEWAY, secret);
        match error {
            GatewayError::Upstream { status, info } => {
                assert_eq!(status, 502);
                assert!(!info.user_message.contains("upstream-secret-token"));
                assert!(!info.original_message.contains("upstream-secret-token"));
            }
            other => panic!("expected upstream error, got {other:?}"),
        }
    }
}
