//! Contract tests protecting the interface between `lanius-core`'s [`Config`]
//! / [`lanius_core::server`] and the desktop GUI application that embeds the
//! gateway as a library (rather than spawning `lanius-cli` as a subprocess).
//!
//! These tests exist to catch breaking changes to the "GUI API surface" —
//! the specific `Config` fields the desktop app maps its settings onto, and
//! the `spawn`/`shutdown` lifecycle it drives — independently of the CLI
//! binary's own subcommands.

use std::time::Duration;

use lanius_core::Config;
use lanius_core::config::DebugMode;

/// Builds a [`Config`] populated the same way the desktop GUI app maps its
/// user-facing settings onto `lanius-core`'s config struct, so the tests
/// below exercise the exact shape of config the GUI produces.
fn build_config_like_desktop_app() -> Config {
    let data_dir = std::path::PathBuf::from("/tmp/lanius-test");

    let mut cfg = Config {
        server_host: "127.0.0.1".to_string(),
        server_port: 8000,
        proxy_api_key: "desktop-key".to_string(),
        region: "us-east-1".to_string(),
        vpn_proxy_url: None,
        first_token_timeout: Duration::from_secs_f64(15.0),
        streaming_read_timeout: Duration::from_secs_f64(300.0),
        truncation_recovery: true,
        log_level: "INFO".to_string(),
        debug_mode: DebugMode::Off,
        ..Config::default()
    };

    cfg.kiro_cli_db_file = Some("/tmp/kiro.db".into());
    cfg.sqlite_readonly = true;

    cfg.debug_dir = data_dir.join("debug_logs");

    cfg
}

/// Verifies that the desktop-style config mapping produces a `Config` that
/// type-checks, passes validation, and derives the expected computed fields
/// (e.g. the region-based API host and absolute debug directory).
#[test]
fn desktop_config_mapping_type_checks_and_validates() {
    let cfg = build_config_like_desktop_app();
    cfg.validate().expect("mapped config must validate");
    assert_eq!(cfg.server_port, 8000);
    assert_eq!(cfg.api_host(), "https://runtime.us-east-1.kiro.dev");
    assert!(cfg.sqlite_readonly);
    assert!(cfg.debug_dir.is_absolute());
}

/// Verifies the in-process gateway lifecycle the desktop app relies on:
/// spawning the server on an OS-assigned ephemeral port, reading back the
/// real bound address, confirming the server answers `/health`, then
/// gracefully shutting it down and confirming the listener is actually
/// closed afterward.
#[tokio::test]
async fn gateway_spawn_handle_lifecycle_matches_desktop_usage() {
    let cfg = Config {
        server_host: "127.0.0.1".to_string(),
        server_port: 0,
        proxy_api_key: "desktop-key".to_string(),
        ..Config::default()
    };

    let handle = lanius_core::server::spawn(cfg)
        .await
        .expect("spawn should succeed on an ephemeral port");

    let addr = handle.local_addr();
    assert_ne!(addr.port(), 0, "OS-assigned port must be reported back");

    let url = format!("http://{addr}/health");
    let status = reqwest::get(&url)
        .await
        .expect("health request should connect")
        .status();
    assert_eq!(status.as_u16(), 200);

    handle
        .shutdown()
        .await
        .expect("graceful shutdown should succeed");

    let after = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    assert!(after.is_err(), "listener should be closed after shutdown");
}
