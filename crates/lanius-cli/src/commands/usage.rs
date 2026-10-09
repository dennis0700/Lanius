//! Implements the `usage [--json]` subcommand: fetches the configured
//! account's raw Kiro credit/quota usage and prints either the raw JSON
//! payload or a human-readable summary.

use std::sync::Arc;

use anyhow::Result;
use lanius_core::Config;
use lanius_core::auth::AuthManager;
use lanius_core::upstream::KiroHttpClient;

/// Implements the `usage [--json]` subcommand: fetches the configured
/// account's raw Kiro credit/quota usage via the same [`AuthManager`] /
/// [`KiroHttpClient`] machinery as the gateway's `GET /usage`/`GET /account`
/// routes (see [`lanius_core::account`]), then prints either the raw JSON
/// payload or a human-readable summary.
///
/// Parameters:
/// - `config`: validated gateway configuration (region, auth, etc.).
/// - `json`: when `true`, print Kiro's raw usage-limits JSON (pretty
///   printed) instead of the formatted summary.
///
/// Side effects: network I/O to obtain/refresh an access token and fetch
/// usage limits; prints the result to stdout.
///
/// Errors if config validation fails, auth/token retrieval fails, or the
/// upstream usage request fails.
pub(crate) async fn run(config: Config, json: bool) -> Result<()> {
    config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;

    let auth = Arc::new(AuthManager::new(config.clone()).map_err(|e| anyhow::anyhow!("{e}"))?);
    let client = KiroHttpClient::new(auth.clone(), &config).map_err(|e| anyhow::anyhow!("{e}"))?;

    let raw = lanius_core::account::fetch_usage(&auth, &client)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fetch usage: {e}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&raw)?);
        return Ok(());
    }

    let summary = lanius_core::account::account_summary(&raw);
    print_account_summary(&summary);
    Ok(())
}

/// Prints the account-summary JSON (as produced by
/// `lanius_core::account::account_summary`) as a human-readable report.
fn print_account_summary(summary: &serde_json::Value) {
    let str_field = |key: &str| summary.get(key).and_then(|v| v.as_str()).unwrap_or("-");
    let f64_field = |key: &str| summary.get(key).and_then(|v| v.as_f64());
    let bool_field = |key: &str| summary.get(key).and_then(|v| v.as_bool()).unwrap_or(false);

    println!("account      : {}", str_field("accountName"));
    println!("plan         : {}", str_field("planType"));
    println!("status       : {}", str_field("accountStatus"));
    if bool_field("isTrial") {
        println!("trial expiry : {}", str_field("trialExpiryDate"));
    }
    let total_quota = f64_field("totalQuota").unwrap_or(0.0);
    let current_usage = f64_field("currentUsage").unwrap_or(0.0);
    let percentage = f64_field("usagePercentage").unwrap_or(0.0);
    println!("usage        : {current_usage:.2} / {total_quota:.2} credits ({percentage:.2}%)");
    if let (Some(bonus_quota), Some(bonus_usage)) =
        (f64_field("bonusQuota"), f64_field("bonusUsage"))
    {
        println!("bonus        : {bonus_usage:.2} / {bonus_quota:.2} credits");
    }
    if let Some(reset_date) = summary.get("resetDate").and_then(|v| v.as_str()) {
        println!("resets       : {reset_date}");
    }
}
