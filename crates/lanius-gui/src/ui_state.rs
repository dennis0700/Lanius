//! Pure conversion functions between backend data types
//! ([`AppConfig`], [`UsageSummary`],
//! [`ProcessedLog`]) and the Slint UI's generated
//! struct types (`ConfigForm`, `UsageView`, `LogRow`).
//!
//! Keeping this mapping logic separate from `controller.rs` means every
//! function here is a plain, synchronous, side-effect-free transformation
//! that can be unit-tested without any UI, async runtime, or I/O — all the
//! stateful orchestration (deciding *when* to call these functions, talking
//! to the server/config/api layers) lives in `controller.rs`, which imports
//! this module to actually build the values it pushes into the UI.

use slint::SharedString;

use crate::api::{UsageSummary, fmt_thousands};
use crate::config::{AppConfig, AuthMethod};
use crate::logs::ProcessedLog;
use crate::{ConfigForm, LogRow, UsageView};

/// Port used to recover the settings form when the user clears/leaves blank
/// the port field, or types something out of the valid TCP port range.
pub const FALLBACK_PORT: u16 = 8000;

/// Converts an `Option<&String>` into a `SharedString`, using Slint's empty
/// `SharedString` for `None` (the settings form has no concept of "unset"
/// text fields, only empty ones).
fn shared(value: Option<&String>) -> SharedString {
    value.map(String::as_str).unwrap_or_default().into()
}

/// Builds the Slint settings-form representation of `config`, for
/// populating the UI when configuration is loaded or reloaded.
pub fn form_from_config(config: &AppConfig) -> ConfigForm {
    ConfigForm {
        auth_method: config.auth_method.as_str().into(),
        refresh_token: shared(config.refresh_token.as_ref()),
        creds_file: shared(config.kiro_creds_file.as_ref()),
        cli_db_file: shared(config.kiro_cli_db_file.as_ref()),
        proxy_api_key: config.proxy_api_key.as_str().into(),
        lan_access: config.server_host == "0.0.0.0",
        server_port: config.server_port.to_string().into(),
        auto_launch: config.auto_launch,
        auto_start_server: config.auto_start_server,
    }
}

/// Parses the free-text port field from the settings form into a valid
/// `u16` TCP port, falling back to [`FALLBACK_PORT`] for anything empty,
/// non-numeric, or outside the valid `1..=65535` range — so a user's
/// in-progress or invalid edit never produces an unusable port (e.g. `0`)
/// for the gateway to bind to.
pub fn parse_port(raw: &str) -> u16 {
    match raw.trim().parse::<u32>() {
        Ok(value) if (1..=65535).contains(&value) => value as u16,
        _ => FALLBACK_PORT,
    }
}

/// Converts a UI text field into `Option<String>`, treating blank/
/// whitespace-only input as "not set" so clearing a credential field in the
/// UI actually clears it in the config rather than saving an empty string.
fn optional(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Produces an updated [`AppConfig`] by applying the user-editable fields
/// from `form` on top of `config`, preserving every field the settings form
/// does not expose (region, timeouts, debug mode, client id, language,
/// etc.) via the `..config.clone()` spread. This is the inverse of
/// [`form_from_config`] for the fields both cover.
pub fn apply_form(config: &AppConfig, form: &ConfigForm) -> AppConfig {
    AppConfig {
        auth_method: AuthMethod::from_str_or_default(form.auth_method.as_str()),
        refresh_token: optional(form.refresh_token.as_str()),
        kiro_creds_file: optional(form.creds_file.as_str()),
        kiro_cli_db_file: optional(form.cli_db_file.as_str()),
        proxy_api_key: form.proxy_api_key.trim().to_string(),
        server_host: if form.lan_access {
            "0.0.0.0".to_string()
        } else {
            "127.0.0.1".to_string()
        },
        server_port: parse_port(form.server_port.as_str()),
        auto_launch: form.auto_launch,
        auto_start_server: form.auto_start_server,
        ..config.clone()
    }
}

/// Builds a "not yet loaded" usage view: `loaded` is always `false` here, so
/// the UI knows to show a loading/placeholder state rather than stale
/// figures, while `loading`/`error` reflect whether a fetch is currently in
/// flight or previously failed.
pub fn usage_placeholder(loading: bool, error: &str) -> UsageView {
    UsageView {
        loaded: false,
        loading,
        error: error.into(),
        ..Default::default()
    }
}

/// Strips a leading "KIRO "/"Kiro "/"kiro " prefix from a plan name for a
/// more compact plan badge (e.g. `"KIRO PRO MAX"` -> `"PRO MAX"`), since the
/// "Kiro" branding is redundant once shown inside a Lanius/Kiro-branded UI.
/// Plan names without that prefix are returned trimmed but otherwise
/// unchanged.
pub fn short_plan(plan: &str) -> &str {
    let trimmed = plan.trim();
    trimmed
        .strip_prefix("KIRO ")
        .or_else(|| trimmed.strip_prefix("Kiro "))
        .or_else(|| trimmed.strip_prefix("kiro "))
        .unwrap_or(trimmed)
        .trim()
}

/// Converts a fetched [`UsageSummary`] into the Slint `UsageView` shown on
/// the dashboard, formatting large numbers with thousands separators via
/// [`fmt_thousands`] and deriving `has_quota` from whether a usage limit is
/// actually known (a plan with no limit info shouldn't render a "0 used"
/// progress bar).
pub fn usage_view(summary: &UsageSummary) -> UsageView {
    UsageView {
        loaded: true,
        loading: false,
        error: SharedString::new(),
        plan: short_plan(&summary.plan).into(),
        percent: summary.percent,
        used_text: fmt_thousands(summary.total_used).into(),
        limit_text: fmt_thousands(summary.total_limit).into(),
        remaining_text: fmt_thousands(summary.remaining()).into(),
        reset_date: summary.reset_date.as_str().into(),
        has_quota: summary.total_limit > 0.0,
        has_overage: summary.has_overage_info,
        overage_enabled: summary.overage_enabled,
    }
}

/// Converts processed log lines into Slint `LogRow` values for binding to
/// the log list model.
pub fn log_rows(processed: &[ProcessedLog]) -> Vec<LogRow> {
    processed
        .iter()
        .map(|line| LogRow {
            time: line.time.as_str().into(),
            text: line.text.as_str().into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_field_rejects_unusable_values() {
        assert_eq!(parse_port("8080"), 8080);
        assert_eq!(parse_port(" 8080 "), 8080);
        assert_eq!(parse_port("65535"), 65535);
        assert_eq!(
            parse_port(""),
            FALLBACK_PORT,
            "empty field must not become 0"
        );
        assert_eq!(parse_port("0"), FALLBACK_PORT);
        assert_eq!(parse_port("65536"), FALLBACK_PORT, "out of range");
        assert_eq!(parse_port("80a"), FALLBACK_PORT);
        assert_eq!(parse_port("-1"), FALLBACK_PORT);
    }

    #[test]
    fn form_round_trips_through_the_config() {
        let mut config = AppConfig {
            auth_method: AuthMethod::CliDb,
            kiro_cli_db_file: Some("/tmp/data.sqlite3".into()),
            proxy_api_key: "sk-key".into(),
            server_host: "0.0.0.0".into(),
            server_port: 9100,
            ..Default::default()
        };
        config.client_id = Some("cid".into());
        config.language = Some("zh".into());

        let form = form_from_config(&config);
        assert_eq!(form.auth_method, "cli_db");
        assert!(form.lan_access, "0.0.0.0 shows as LAN access");
        assert_eq!(form.server_port, "9100");

        let restored = apply_form(&config, &form);
        assert_eq!(restored, config, "a round trip must not change anything");
    }

    #[test]
    fn applying_the_form_preserves_fields_the_ui_does_not_expose() {
        let config = AppConfig {
            kiro_region: "eu-central-1".into(),
            first_token_timeout: 42.0,
            debug_mode: "all".into(),
            client_id: Some("keep-me".into()),
            language: Some("ja".into()),
            vpn_proxy_url: Some("socks5://127.0.0.1:1080".into()),
            ..Default::default()
        };
        let mut form = form_from_config(&config);
        form.proxy_api_key = "sk-new".into();

        let updated = apply_form(&config, &form);
        assert_eq!(updated.proxy_api_key, "sk-new");
        assert_eq!(updated.kiro_region, "eu-central-1");
        assert_eq!(updated.first_token_timeout, 42.0);
        assert_eq!(updated.debug_mode, "all");
        assert_eq!(updated.client_id.as_deref(), Some("keep-me"));
        assert_eq!(updated.language.as_deref(), Some("ja"));
        assert_eq!(
            updated.vpn_proxy_url.as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
    }

    #[test]
    fn blank_credential_fields_become_none() {
        let config = AppConfig::default();
        let mut form = form_from_config(&config);
        form.refresh_token = "   ".into();
        form.creds_file = "".into();
        let updated = apply_form(&config, &form);
        assert_eq!(updated.refresh_token, None);
        assert_eq!(updated.kiro_creds_file, None);
    }

    #[test]
    fn lan_toggle_maps_to_the_bind_address() {
        let config = AppConfig::default();
        let mut form = form_from_config(&config);
        form.lan_access = true;
        assert_eq!(apply_form(&config, &form).server_host, "0.0.0.0");
        form.lan_access = false;
        assert_eq!(apply_form(&config, &form).server_host, "127.0.0.1");
    }

    #[test]
    fn plan_badge_drops_the_kiro_prefix() {
        assert_eq!(short_plan("KIRO PRO MAX"), "PRO MAX");
        assert_eq!(short_plan("Kiro Power"), "Power");
        assert_eq!(short_plan("  KIRO PRO  "), "PRO");
        assert_eq!(short_plan("PRO MAX"), "PRO MAX");
        assert_eq!(short_plan("KIROSOMETHING"), "KIROSOMETHING");
        assert_eq!(short_plan(""), "");
    }

    #[test]
    fn usage_view_formats_the_status_card() {
        let summary = UsageSummary {
            total_limit: 1700.0,
            total_used: 400.0,
            percent: 24,
            plan: "PRO".into(),
            reset_date: "2026-10-01".into(),
            has_overage_info: true,
            overage_enabled: true,
            ..Default::default()
        };
        let view = usage_view(&summary);
        assert!(view.loaded);
        assert_eq!(view.used_text, "400");
        assert_eq!(view.limit_text, "1,700");
        assert_eq!(view.remaining_text, "1,300");
        assert_eq!(view.percent, 24);
        assert_eq!(view.plan, "PRO");
        assert_eq!(view.reset_date, "2026-10-01");
        assert!(view.has_quota && view.has_overage && view.overage_enabled);

        let empty = usage_view(&UsageSummary::default());
        assert!(!empty.has_quota);
    }

    #[test]
    fn placeholder_usage_is_not_marked_loaded() {
        let view = usage_placeholder(true, "");
        assert!(!view.loaded);
        assert!(view.loading);
        assert!(!view.has_quota);
    }

    #[test]
    fn usage_view_loading_can_be_overridden_without_touching_other_fields() {
        let summary = UsageSummary {
            total_used: 400.0,
            total_limit: 1_700.0,
            plan: "PRO".into(),
            reset_date: "2026-10-01".into(),
            ..Default::default()
        };

        let mut view = usage_view(&summary);
        assert!(!view.loading, "usage_view must default to loading: false");

        view.loading = true;
        assert!(
            view.loaded,
            "existing figures must stay visible during a manual refresh"
        );
        assert_eq!(view.used_text, "400");
        assert_eq!(view.limit_text, "1,700");
        assert_eq!(view.plan, "PRO");
    }

    #[test]
    fn log_rows_preserve_gutter_and_message() {
        let processed = vec![ProcessedLog {
            time: "18:11:11".into(),
            text: "started".into(),
        }];
        let rows = log_rows(&processed);
        assert_eq!(rows[0].time, "18:11:11");
        assert_eq!(rows[0].text, "started");
    }
}
