//! HTTP client for the locally embedded gateway's own API, plus a couple of
//! small, self-contained data types shared with the UI.
//!
//! Everything in this module talks to `http://{host}:{port}/...` on the
//! same machine (the gateway `server.rs` starts in-process) — there is no
//! remote networking here. `controller.rs` is the sole caller: it uses
//! [`wait_for_health`] after (re)starting the gateway, [`fetch_models`] to
//! populate the model list, and [`fetch_usage`]/[`parse_usage`] to populate
//! the usage/credit dashboard (whose fields are then converted to the
//! Slint `UsageView` by `ui_state.rs`).

use std::time::{Duration, Instant};

use serde_json::Value;

/// Builds a fresh `reqwest::Client` configured for talking to the local
/// gateway: a 20-second timeout and no system/env proxy (`.no_proxy()`),
/// since the target is always `127.0.0.1`/the configured LAN host and
/// should never be routed through an external HTTP proxy. Falls back to
/// `Client::default()` if building with these options somehow fails, rather
/// than panicking.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .no_proxy()
        .build()
        .unwrap_or_default()
}

/// Polls the gateway's `/health` endpoint every 500ms until it responds
/// with a successful status or `timeout` elapses, returning whether it
/// became healthy in time. Used after (re)starting the embedded gateway,
/// since `lanius_core::server::spawn` returning successfully only means the
/// listener is bound, not that the gateway has finished any internal
/// warmup.
///
/// # Examples
///
/// ```ignore
/// use std::time::Duration;
/// let healthy = crate::api::wait_for_health("127.0.0.1", 8000, Duration::from_secs(20)).await;
/// if !healthy {
///     tracing::warn!("gateway did not become healthy in time");
/// }
/// ```
pub async fn wait_for_health(host: &str, port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    let client = client();
    while start.elapsed() < timeout {
        if let Ok(response) = client
            .get(format!("http://{host}:{port}/health"))
            .send()
            .await
        {
            if response.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// A single model entry as shown in the GUI's model list, derived from the
/// gateway's `/v1/models` response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub rate_text: String,
    pub description: String,
    /// Whether the gateway reports native thinking support for the model
    /// (the `supports_thinking` extension field of `/v1/models`).
    pub supports_thinking: bool,
}

/// Splits a model's raw `description` string (as returned by the gateway)
/// into its credit-rate prefix and human-readable description.
///
/// The gateway formats these as `"<rate> \u{2013} <description>"` (an en
/// dash, not a hyphen, is the separator — chosen because model descriptions
/// may legitimately contain hyphens). If no en-dash separator is found but
/// the whole string looks like a rate on its own (ends with `"x credits"`),
/// it's treated as rate-only with an empty description; otherwise the
/// entire string is treated as the description with no rate.
fn split_model_description(raw: &str) -> (String, String) {
    const SEPARATOR: &str = " \u{2013} ";
    if let Some((rate, description)) = raw.split_once(SEPARATOR) {
        return (rate.to_string(), description.to_string());
    }
    if raw.ends_with("x credits") {
        (raw.to_string(), String::new())
    } else {
        (String::new(), raw.to_string())
    }
}

/// Fetches and parses the gateway's `/v1/models` list into [`ModelInfo`]
/// rows, filtering out any model id that starts with `"auto"` (case
/// insensitive) since Lanius's model picker is meant to show only concrete,
/// selectable models rather than an auto-routing pseudo-model. Returns an
/// error string (not a typed error) on any HTTP-level or non-2xx failure,
/// matching this module's convention of surfacing errors as plain text
/// suitable for direct display or logging.
///
/// # Examples
///
/// ```ignore
/// let models = crate::api::fetch_models("127.0.0.1", 8000, "sk-example").await?;
/// for model in &models {
///     println!("{} {}", model.id, model.rate_text);
/// }
/// ```
pub async fn fetch_models(host: &str, port: u16, api_key: &str) -> Result<Vec<ModelInfo>, String> {
    let response = client()
        .get(format!("http://{host}:{port}/v1/models"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }

    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    Ok(body
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?;
                    if id.to_ascii_lowercase().starts_with("auto") {
                        return None;
                    }
                    let (rate_text, description) = item
                        .get("description")
                        .and_then(Value::as_str)
                        .map(split_model_description)
                        .unwrap_or_default();
                    let supports_thinking = item
                        .get("supports_thinking")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    Some(ModelInfo {
                        id: id.to_string(),
                        rate_text,
                        description,
                        supports_thinking,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Aggregated usage/quota figures for the currently configured account, as
/// shown on the dashboard's usage card. Combines trial, plan (free), and
/// bonus credit buckets into `total_limit`/`total_used`; see [`parse_usage`]
/// for how each field is derived from the raw gateway response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSummary {
    pub plan: String,
    pub email: String,
    pub provider: String,
    pub status: String,
    pub is_trial: bool,
    pub trial_limit: f64,
    pub trial_used: f64,
    pub free_limit: f64,
    pub free_used: f64,
    pub bonus_limit: f64,
    pub bonus_used: f64,
    pub total_limit: f64,
    pub total_used: f64,
    pub percent: i32,
    pub reset_date: String,
    pub overage_enabled: bool,
    pub has_overage_info: bool,
    pub trial_expiry: String,
    pub subscription_expiry: String,
}

impl UsageSummary {
    /// Remaining usage (`total_limit - total_used`), clamped to never go
    /// negative even if usage momentarily exceeds the reported limit (e.g.
    /// due to a stale/racy limit figure).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let usage = crate::api::UsageSummary { total_limit: 1000.0, total_used: 250.0, ..Default::default() };
    /// assert_eq!(usage.remaining(), 750.0);
    /// ```
    pub fn remaining(&self) -> f64 {
        (self.total_limit - self.total_used).max(0.0)
    }

    /// Formats the tray menu's "Credit: used / limit (percent%)" line, or
    /// `None` if there is no usable quota to show (`total_limit <= 0.0`,
    /// e.g. before usage has ever been fetched successfully) — callers keep
    /// the tray's previous label in that case rather than replacing it with
    /// a meaningless "0 / 0" line.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let usage = crate::api::UsageSummary { total_limit: 1700.0, total_used: 400.0, percent: 24, ..Default::default() };
    /// assert_eq!(usage.tray_label().as_deref(), Some("Credit: 400 / 1,700 (24%)"));
    /// assert!(crate::api::UsageSummary::default().tray_label().is_none());
    /// ```
    pub fn tray_label(&self) -> Option<String> {
        if self.total_limit <= 0.0 {
            return None;
        }
        Some(format!(
            "Credit: {} / {} ({}%)",
            fmt_thousands(self.total_used),
            fmt_thousands(self.total_limit),
            self.percent
        ))
    }
}

/// Formats a number with thousands separators to match JavaScript's
/// `toLocaleString()` behavior for the `en-US` locale (e.g. `1234.6` ->
/// `"1,235"`), since the design this UI was ported from used that
/// formatting. The value is rounded to the nearest integer first, then
/// grouped into comma-separated triples from the right; a negative sign is
/// re-applied after formatting the absolute value so grouping logic doesn't
/// need to special-case it.
///
/// # Examples
///
/// ```ignore
/// assert_eq!(crate::api::fmt_thousands(1234.6), "1,235");
/// assert_eq!(crate::api::fmt_thousands(1_234_567.0), "1,234,567");
/// ```
pub fn fmt_thousands(value: f64) -> String {
    let rounded = value.round().abs() as u64;
    let digits = rounded.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if value < 0.0 { format!("-{out}") } else { out }
}

/// Fetches and parses the gateway's `/usage` endpoint into a
/// [`UsageSummary`]. Returns an error string on any HTTP-level or non-2xx
/// failure; actual field extraction/normalization is delegated to
/// [`parse_usage`] so that logic can be unit-tested against raw JSON
/// fixtures without a real HTTP call.
///
/// # Examples
///
/// ```ignore
/// let usage = crate::api::fetch_usage("127.0.0.1", 8000, "sk-example").await?;
/// println!("{}% used, {} remaining", usage.percent, usage.remaining());
/// ```
pub async fn fetch_usage(host: &str, port: u16, api_key: &str) -> Result<UsageSummary, String> {
    let response = client()
        .get(format!("http://{host}:{port}/usage"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }

    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    Ok(parse_usage(&body))
}

/// Normalizes the gateway's raw `/usage` JSON payload into a
/// [`UsageSummary`].
///
/// The upstream payload models usage as a list of "breakdowns"
/// (`usageBreakdownList`): by convention here, the first entry is the
/// primary/free-trial-eligible plan bucket (which may itself carry a nested
/// `freeTrialInfo` sub-bucket), and the second, if present, is a bonus
/// credit bucket. Each bucket may report its limit/usage either as an
/// integer (`usageLimit`/`currentUsage`) or, when available, as a more
/// precise float (`usageLimitWithPrecision`/`currentUsageWithPrecision`);
/// the `pick` closure prefers the precise value whenever present. The three
/// buckets (trial, free/primary, bonus) are simply summed into the reported
/// `total_limit`/`total_used`, and `percent` is derived from that ratio
/// (capped at 100%, since displaying over 100% used would look like a
/// bug). Every other field falls back to an empty/default value if absent,
/// so a partial or unexpected payload shape never causes a parse failure —
/// only missing data.
pub(crate) fn parse_usage(body: &Value) -> UsageSummary {
    let breakdowns = body
        .get("usageBreakdownList")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let primary = breakdowns.first();
    let bonus = breakdowns.get(1);
    let free_trial = primary.and_then(|b| b.get("freeTrialInfo"));

    let pick = |node: Option<&Value>, precise: &str, plain: &str| -> f64 {
        node.and_then(|n| {
            n.get(precise)
                .and_then(Value::as_f64)
                .or_else(|| n.get(plain).and_then(Value::as_f64))
        })
        .unwrap_or(0.0)
    };

    let trial_limit = pick(free_trial, "usageLimitWithPrecision", "usageLimit");
    let trial_used = pick(free_trial, "currentUsageWithPrecision", "currentUsage");
    let free_limit = pick(primary, "usageLimitWithPrecision", "usageLimit");
    let free_used = pick(primary, "currentUsageWithPrecision", "currentUsage");
    let bonus_limit = pick(bonus, "usageLimitWithPrecision", "usageLimit");
    let bonus_used = pick(bonus, "currentUsageWithPrecision", "currentUsage");

    let total_limit = trial_limit + free_limit + bonus_limit;
    let total_used = trial_used + free_used + bonus_used;
    let percent = if total_limit > 0.0 {
        ((total_used / total_limit) * 100.0).round().min(100.0) as i32
    } else {
        0
    };

    let string_at = |path: [&str; 2]| -> String {
        body.get(path[0])
            .and_then(|n| n.get(path[1]))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let is_trial = free_trial
        .and_then(|n| n.get("freeTrialStatus"))
        .and_then(Value::as_str)
        == Some("ACTIVE");

    let subscription_status = string_at(["subscriptionInfo", "status"]);
    // An active free trial takes display priority over whatever the
    // underlying subscription status says (a trialing user's subscription
    // record may say "ACTIVE" too, but "Trial" is the more useful label).
    let status = if is_trial {
        "Trial".to_string()
    } else if subscription_status.is_empty() {
        "Active".to_string()
    } else {
        subscription_status
    };

    let reset_date = body
        .get("nextDateReset")
        .and_then(epoch_seconds)
        .map(format_epoch_seconds)
        .or_else(|| {
            primary
                .and_then(|b| b.get("resetDate"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let overage_status = string_at(["overageConfiguration", "overageStatus"]);

    let subscription_expiry = {
        let primary_expiry = string_at(["subscriptionInfo", "expiryDate"]);
        if primary_expiry.is_empty() {
            string_at(["subscriptionInfo", "subscriptionExpiryDate"])
        } else {
            primary_expiry
        }
    };

    UsageSummary {
        plan: string_at(["subscriptionInfo", "subscriptionTitle"]),
        email: string_at(["userInfo", "email"]),
        provider: string_at(["userInfo", "provider"]),
        status,
        is_trial,
        trial_limit,
        trial_used,
        free_limit,
        free_used,
        bonus_limit,
        bonus_used,
        total_limit,
        total_used,
        percent,
        reset_date,
        overage_enabled: overage_status == "ENABLED",
        has_overage_info: !overage_status.is_empty(),
        trial_expiry: free_trial
            .and_then(|n| n.get("freeTrialExpiry"))
            .and_then(epoch_seconds)
            .map(format_epoch_seconds)
            .unwrap_or_default(),
        subscription_expiry,
    }
}

/// Reads a Unix epoch-seconds timestamp from a JSON value that may be
/// encoded as either an integer or a float, normalizing to `i64`.
fn epoch_seconds(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|seconds| seconds as i64))
}

/// Converts a Unix epoch-seconds timestamp into a `YYYY-MM-DD` calendar
/// date string (UTC), using Howard Hinnant's well-known
/// days-since-epoch-to-civil-date algorithm rather than pulling in a full
/// date/time library dependency just for this one conversion. `days` is the
/// number of whole days since the epoch (via Euclidean division, so it
/// rounds toward negative infinity for negative timestamps rather than
/// toward zero); the `era`/`doe`/`yoe`/`doy`/`mp` intermediate values follow
/// that algorithm's civil-calendar decomposition exactly and are not
/// meaningful in isolation.
fn format_epoch_seconds(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

/// Returns the running build's version string, taken from the crate's
/// `CARGO_PKG_VERSION` at compile time.
///
/// # Examples
///
/// ```ignore
/// let version = crate::api::app_version();
/// assert_eq!(version, env!("CARGO_PKG_VERSION"));
/// ```
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_description_is_split_back_into_rate_and_text() {
        assert_eq!(
            split_model_description(
                "1.30x credits \u{2013} Claude Sonnet 4 model with 1M context window"
            ),
            (
                "1.30x credits".to_string(),
                "Claude Sonnet 4 model with 1M context window".to_string()
            )
        );
        assert_eq!(
            split_model_description("2.20x credits"),
            ("2.20x credits".to_string(), String::new())
        );
        assert_eq!(
            split_model_description("Claude model via Kiro API"),
            (String::new(), "Claude model via Kiro API".to_string())
        );
    }

    #[test]
    fn thousands_separator_matches_js_to_locale_string() {
        assert_eq!(fmt_thousands(0.0), "0");
        assert_eq!(fmt_thousands(7.0), "7");
        assert_eq!(fmt_thousands(999.0), "999");
        assert_eq!(fmt_thousands(1000.0), "1,000");
        assert_eq!(fmt_thousands(1234.4), "1,234");
        assert_eq!(fmt_thousands(1234.6), "1,235");
        assert_eq!(fmt_thousands(1_234_567.0), "1,234,567");
    }

    #[test]
    fn epoch_formatting_is_a_calendar_date() {
        assert_eq!(format_epoch_seconds(0), "1970-01-01");
        assert_eq!(format_epoch_seconds(1_700_000_000), "2023-11-14");
        assert_eq!(format_epoch_seconds(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn usage_totals_sum_trial_plan_and_bonus_credits() {
        let body: Value = serde_json::from_str(
            r#"{
                "usageBreakdownList": [
                    {
                        "usageLimitWithPrecision": 1000.0,
                        "currentUsageWithPrecision": 250.0,
                        "freeTrialInfo": {
                            "usageLimit": 500,
                            "currentUsage": 100,
                            "freeTrialStatus": "ACTIVE",
                            "freeTrialExpiry": 1700000000
                        }
                    },
                    { "usageLimit": 200, "currentUsage": 50 }
                ],
                "userInfo": { "email": "a@b.c", "provider": "google" },
                "subscriptionInfo": { "subscriptionTitle": "PRO", "status": "ACTIVE" },
                "overageConfiguration": { "overageStatus": "ENABLED" },
                "nextDateReset": 1700000000
            }"#,
        )
        .unwrap();

        let usage = parse_usage(&body);
        assert_eq!(usage.trial_limit, 500.0);
        assert_eq!(usage.free_limit, 1000.0);
        assert_eq!(usage.bonus_limit, 200.0);
        assert_eq!(usage.total_limit, 1700.0);
        assert_eq!(usage.total_used, 400.0);
        assert_eq!(usage.percent, 24, "400/1700 rounds to 24%");
        assert_eq!(usage.remaining(), 1300.0);
        assert!(usage.is_trial);
        assert_eq!(
            usage.status, "Trial",
            "an active trial overrides the plan status"
        );
        assert_eq!(usage.plan, "PRO");
        assert_eq!(usage.email, "a@b.c");
        assert!(usage.overage_enabled);
        assert_eq!(usage.reset_date, "2023-11-14");
        assert_eq!(
            usage.tray_label().as_deref(),
            Some("Credit: 400 / 1,700 (24%)")
        );
    }

    #[test]
    fn float_timestamps_are_accepted() {
        let body = serde_json::json!({
            "nextDateReset": 1790812800.0,
            "usageBreakdownList": [{
                "usageLimit": 5000,
                "currentUsage": 1000,
                "freeTrialInfo": { "freeTrialExpiry": 1700000000.0 }
            }]
        });
        let usage = parse_usage(&body);
        assert_eq!(usage.reset_date, "2026-10-01");
        assert_eq!(usage.trial_expiry, "2023-11-14");
    }

    #[test]
    fn usage_parsing_survives_an_empty_payload() {
        let usage = parse_usage(&serde_json::json!({}));
        assert_eq!(
            usage,
            UsageSummary {
                status: "Active".into(),
                ..Default::default()
            }
        );
        assert_eq!(usage.percent, 0);
        assert!(usage.tray_label().is_none(), "no quota means no tray label");
        assert!(!usage.has_overage_info);
    }

    #[test]
    fn precise_values_win_over_rounded_ones() {
        let body = serde_json::json!({
            "usageBreakdownList": [{
                "usageLimit": 100,
                "usageLimitWithPrecision": 100.5,
                "currentUsage": 10,
                "currentUsageWithPrecision": 10.25
            }]
        });
        let usage = parse_usage(&body);
        assert_eq!(usage.free_limit, 100.5);
        assert_eq!(usage.free_used, 10.25);
    }
}
