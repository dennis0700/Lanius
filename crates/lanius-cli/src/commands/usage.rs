//! Implements the `usage [--json]` subcommand: fetches the configured
//! account's raw Kiro credit/quota usage and prints either the raw JSON
//! payload or a human-readable summary. When `KIRO_API_KEY` holds several
//! comma-separated keys, the usage of every key is fetched and printed.

use std::sync::Arc;

use anyhow::Result;
use lanius_core::Config;
use lanius_core::auth::AuthManager;
use lanius_core::upstream::KiroHttpClient;
use serde_json::{Value, json};

/// Implements the `usage [--json]` subcommand: fetches the configured
/// account's raw Kiro credit/quota usage via the same [`AuthManager`] /
/// [`KiroHttpClient`] machinery as the gateway's `GET /usage`/`GET /account`
/// routes (see [`lanius_core::account`]), then prints either the raw JSON
/// payload or a human-readable summary.
///
/// With more than one key in `KIRO_API_KEY`, every key is queried
/// concurrently and reported separately (identified by its position and a
/// masked form of the key); `--json` then prints an array of
/// `{"index", "key", "usage"}` / `{"index", "key", "error"}` entries. A
/// failing key does not hide the others; the command only fails if every
/// key fails.
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
/// upstream usage request fails (for every key, in multi-key mode).
pub(crate) async fn run(config: Config, json: bool) -> Result<()> {
    config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;

    let keys: Vec<String> = config.api_keys().map(str::to_owned).collect();
    if keys.len() > 1 {
        return run_multi_key(&config, &keys, json).await;
    }

    let raw = fetch(config).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&raw)?);
        return Ok(());
    }

    let summary = lanius_core::account::account_summary(&raw);
    print_account_summary(&summary);
    Ok(())
}

/// Fetches raw usage for the account described by `config`.
async fn fetch(config: Config) -> Result<Value> {
    let auth = Arc::new(AuthManager::new(config.clone()).map_err(|e| anyhow::anyhow!("{e}"))?);
    let client = KiroHttpClient::new(auth.clone(), &config).map_err(|e| anyhow::anyhow!("{e}"))?;
    lanius_core::account::fetch_usage(&auth, &client)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fetch usage: {e}"))
}

/// Queries every configured API key concurrently and prints each result.
async fn run_multi_key(config: &Config, keys: &[String], json: bool) -> Result<()> {
    let results = futures_util::future::join_all(keys.iter().map(|key| {
        let config = Config {
            kiro_api_key: Some(key.clone()),
            ..config.clone()
        };
        fetch(config)
    }))
    .await;
    let failures = results.iter().filter(|result| result.is_err()).count();

    if json {
        let entries: Vec<Value> = keys
            .iter()
            .zip(&results)
            .enumerate()
            .map(|(index, (key, result))| {
                let mut entry = json!({"index": index + 1, "key": mask_key(key)});
                match result {
                    Ok(raw) => entry["usage"] = raw.clone(),
                    Err(error) => entry["error"] = json!(error.to_string()),
                }
                entry
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for (index, (key, result)) in keys.iter().zip(&results).enumerate() {
            if index > 0 {
                println!();
            }
            println!("[key {}/{}] {}", index + 1, keys.len(), mask_key(key));
            match result {
                Ok(raw) => print_account_summary(&lanius_core::account::account_summary(raw)),
                Err(error) => println!("error        : {error}"),
            }
        }
    }

    if failures == keys.len() {
        anyhow::bail!("failed to fetch usage for all {} API keys", keys.len());
    }
    Ok(())
}

/// Masks an API key for display, keeping only a short prefix and the last
/// four characters (e.g. `ksk_…abcd`). Short keys are fully masked.
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 12 {
        return "…".to_owned();
    }
    let prefix: String = chars.iter().take(4).collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}…{suffix}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_key_keeps_only_prefix_and_last_four() {
        assert_eq!(mask_key("ksk_0123456789abcdef"), "ksk_…cdef");
        assert_eq!(mask_key("short"), "…");
        assert_eq!(mask_key("ksk_密钥密钥密钥密钥密钥"), "ksk_…密钥密钥");
    }
}
