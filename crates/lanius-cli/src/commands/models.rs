//! Implements the `models [--json]` subcommand: fetches the live Kiro model
//! catalog (falling back silently to the built-in list if that fails) and
//! resolves it the same way the gateway's `GET /v1/models` route does, then
//! prints the resulting model list either as JSON or a formatted table.

use std::sync::Arc;

use anyhow::Result;
use lanius_core::Config;
use lanius_core::auth::AuthManager;
use lanius_core::model::{ModelInfoCache, ModelResolver, fetch_available_models};

/// Implements the `models [--json]` subcommand: fetches the live Kiro model
/// catalog (falling back silently to the built-in list if that fails) and
/// resolves it the same way the gateway's `GET /v1/models` route does (see
/// [`lanius_core::model::ModelResolver`]), then prints the resulting model
/// list either as JSON or a formatted table.
///
/// Parameters:
/// - `config`: validated gateway configuration (region, auth, aliases, etc.).
/// - `json`: when `true`, print the model list as a JSON array instead of a
///   formatted table.
///
/// Side effects: network I/O to obtain/refresh an access token and fetch
/// the live model catalog; prints the result to stdout.
///
/// Errors if config validation fails or auth/token retrieval fails.
pub(crate) async fn run(config: Config, json: bool) -> Result<()> {
    config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;

    let auth = Arc::new(AuthManager::new(config.clone()).map_err(|e| anyhow::anyhow!("{e}"))?);

    let cache = ModelInfoCache::default();
    cache.load_fallback();
    match fetch_available_models(auth, &config).await {
        Some(catalog) if !catalog.is_empty() => cache.update(catalog),
        _ => tracing::warn!("could not fetch the live model catalog; using the fallback list"),
    }

    let resolver = ModelResolver::from_config(cache, &config);
    let details = resolver.get_available_model_details();

    if json {
        let data: Vec<serde_json::Value> = details
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "description": d.description,
                    "rate_multiplier": d.rate_multiplier,
                    "supports_thinking": d.supports_thinking,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let id_width = details
        .iter()
        .map(|d| d.id.chars().count())
        .max()
        .unwrap_or(2)
        .max("MODEL".len());
    println!(
        "{:<id_width$}  {:>6}  {:<9}  DESCRIPTION",
        "MODEL", "RATE", "THINKING"
    );
    for d in &details {
        let rate = d
            .rate_multiplier
            .map(|r| format!("{r:.2}x"))
            .unwrap_or_else(|| "-".to_string());
        let thinking = if d.supports_thinking { "yes" } else { "no" };
        let description = d.description.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {rate:>6}  {thinking:<9}  {description}",
            d.id
        );
    }
    println!("\n{} models available", details.len());
    Ok(())
}
