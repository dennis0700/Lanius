//! Generated-style glue code that populates the Slint `Tr` (translations)
//! global from a runtime string lookup.
//!
//! This file is hand-maintained but written in a generated-code style: it is
//! a flat, mechanical list of `tr.set_<field>(lookup("<jsonKey>"))` calls,
//! one per translatable UI string declared on the Slint `Tr` global (see the
//! `.slint` UI definitions). It exists because Slint's `Tr` global exposes a
//! fixed set of typed properties, while the actual translated strings live
//! in the JSON tables loaded by [`crate::i18n::Translations`]; this module
//! is the bridge between the two, mapping each Slint property to its
//! corresponding JSON key.
//!
//! It is regenerated/extended whenever a new translatable string is added to
//! the Slint UI and the `i18n/en.json` / `i18n/zh.json` tables — every
//! `tr.set_*` call here must have a matching property on `Tr` and a matching
//! key in every language table (enforced by a test in `i18n.rs`). Call sites
//! ([`crate::controller::Controller::refresh_translations`]) invoke
//! [`apply`] once per language change, passing a closure that performs the
//! actual key -> string lookup (with English fallback).

use crate::Tr;
use slint::SharedString;

/// Populates every translatable property on the Slint `Tr` global by calling
/// `lookup` with each property's corresponding JSON translation key.
///
/// `lookup` is expected to resolve a JSON key to its localized string (with
/// fallback to English / to the key itself for unknown keys — see
/// [`crate::i18n::Translations::get`]); this function performs no fallback
/// logic of its own. This directly mutates the live `Tr` global, so any UI
/// bound to `Tr`'s properties will refresh immediately.
pub fn apply(tr: &Tr, lookup: impl Fn(&str) -> SharedString) {
    tr.set_dashboard(lookup("dashboard"));
    tr.set_tab_settings(lookup("tabSettings"));
    tr.set_tab_logs(lookup("tabLogs"));
    tr.set_start_server(lookup("startServer"));
    tr.set_stop_server(lookup("stopServer"));
    tr.set_starting(lookup("starting"));
    tr.set_stopping(lookup("stopping"));
    tr.set_start_server_failed(lookup("startServerFailed"));
    tr.set_status_stopped(lookup("statusStopped"));
    tr.set_status_running(lookup("statusRunning"));
    tr.set_auth_refresh_token(lookup("authRefreshToken"));
    tr.set_auth_creds_file(lookup("authCredsFile"));
    tr.set_auth_cli_db(lookup("authCliDb"));
    tr.set_refresh_token(lookup("refreshToken"));
    tr.set_proxy_api_key(lookup("proxyApiKey"));
    tr.set_save_failed(lookup("saveFailed"));
    tr.set_loading_config(lookup("loadingConfig"));
    tr.set_language(lookup("language"));
    tr.set_configuration_error(lookup("configurationError"));
    tr.set_dashboard_desc(lookup("dashboardDesc"));
    tr.set_logs_desc(lookup("logsDesc"));
    tr.set_models_desc(lookup("modelsDesc"));
    tr.set_gateway_status(lookup("gatewayStatus"));
    tr.set_offline(lookup("offline"));
    tr.set_online(lookup("online"));
    tr.set_stopped(lookup("stopped"));
    tr.set_ready_to_initialize(lookup("readyToInitialize"));
    tr.set_api_examples(lookup("apiExamples"));
    tr.set_copy(lookup("copy"));
    tr.set_copied(lookup("copied"));
    tr.set_remaining_quota(lookup("remainingQuota"));
    tr.set_sqlite_database_path(lookup("sqliteDatabasePath"));
    tr.set_credentials_file_path(lookup("credentialsFilePath"));
    tr.set_clients_include_key(lookup("clientsIncludeKey"));
    tr.set_saved_successfully(lookup("savedSuccessfully"));
    tr.set_saving_changes(lookup("savingChanges"));
    tr.set_save_configuration(lookup("saveConfiguration"));
    tr.set_system_output(lookup("systemOutput"));
    tr.set_events_captured(lookup("eventsCaptured"));
    tr.set_live(lookup("live"));
    tr.set_paused(lookup("paused"));
    tr.set_no_logs_available(lookup("noLogsAvailable"));
    tr.set_enable_lan_access(lookup("enableLanAccess"));
    tr.set_lan_access_desc(lookup("lanAccessDesc"));
    tr.set_auto_launch(lookup("autoLaunch"));
    tr.set_auto_launch_desc(lookup("autoLaunchDesc"));
    tr.set_auto_start_server(lookup("autoStartServer"));
    tr.set_auto_start_server_desc(lookup("autoStartServerDesc"));
    tr.set_server_port(lookup("serverPort"));
    tr.set_server_port_desc(lookup("serverPortDesc"));
    tr.set_config_change_hint(lookup("configChangeHint"));
    tr.set_restart_server(lookup("restartServer"));
    tr.set_tray_show_window(lookup("trayShowWindow"));
    tr.set_tray_hide_window(lookup("trayHideWindow"));
    tr.set_tray_quit(lookup("trayQuit"));
    tr.set_tooltip_auth_cli_db(lookup("tooltip_auth_cli_db"));
    tr.set_tooltip_auth_creds_file(lookup("tooltip_auth_creds_file"));
    tr.set_tooltip_auth_refresh_token(lookup("tooltip_auth_refresh_token"));
    tr.set_tooltip_proxy_api_key(lookup("tooltip_proxy_api_key"));
    tr.set_tooltip_generate(lookup("tooltip_generate"));
    tr.set_tooltip_show_key(lookup("tooltip_show_key"));
    tr.set_tooltip_hide_key(lookup("tooltip_hide_key"));
    tr.set_tooltip_copy_key(lookup("tooltip_copy_key"));
    tr.set_tooltip_save(lookup("tooltip_save"));
    tr.set_usage_resets(lookup("usageResets"));
    tr.set_usage_overage(lookup("usageOverage"));
    tr.set_usage_overage_on(lookup("usageOverageOn"));
    tr.set_usage_overage_off(lookup("usageOverageOff"));
    tr.set_supports_thinking(lookup("supportsThinking"));
    tr.set_supported_models(lookup("supportedModels"));
    tr.set_auto_check_updates(lookup("autoCheckUpdates"));
    tr.set_auto_check_updates_desc(lookup("autoCheckUpdatesDesc"));
    tr.set_updates_section(lookup("updatesSection"));
    tr.set_update_check_now(lookup("updateCheckNow"));
    tr.set_update_checking(lookup("updateChecking"));
    tr.set_update_up_to_date(lookup("updateUpToDate"));
    tr.set_update_available(lookup("updateAvailable"));
    tr.set_update_now(lookup("updateNow"));
    tr.set_update_installing(lookup("updateInstalling"));
    tr.set_update_downloading(lookup("updateDownloading"));
    tr.set_update_failed(lookup("updateFailed"));
    tr.set_update_view_release(lookup("updateViewRelease"));
    tr.set_update_current_version(lookup("updateCurrentVersion"));
}
