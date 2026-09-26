//! Headless UI smoke tests for the Slint `MainWindow`.
//!
//! Exercises the generated Slint UI (`MainWindow` and its callbacks/models)
//! against real `Controller`/`ui_state`/`i18n` conversions, using
//! `i_slint_backend_testing`'s headless backend so these tests can run
//! without a real display. Kept in `#[cfg(test)]` only (see `main.rs`) —
//! this module does not ship in release builds. Individual test functions
//! are not documented in detail here; see each test body for what it
//! exercises.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::platform::PointerEventButton;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::api::UsageSummary;
use crate::config::AppConfig;
use crate::examples::{ApiFlavor, Snippet};
use crate::i18n::Translations;
use crate::logs;
use crate::ui_state;
use crate::{LogRow, MainWindow, ModelRow, Tr};

#[test]
fn ui_renders_every_view_and_reports_every_callback() {
    i_slint_backend_testing::init_no_event_loop();

    let ui = MainWindow::new().expect("window must be constructible");
    ui.window().set_size(slint::LogicalSize::new(1180.0, 820.0));
    ui.show().expect("headless window must show");

    let mut translations = Translations::load();
    translations.set_language("zh");
    let table = translations.snapshot();
    crate::tr_generated::apply(&ui.global::<Tr>(), |key| {
        SharedString::from(table.get(key).map(String::as_str).unwrap_or(key))
    });
    assert_eq!(
        ui.global::<Tr>().get_tab_settings(),
        "设置",
        "Tr global must carry the selected language"
    );

    let config = AppConfig {
        proxy_api_key: "sk-test".into(),
        server_port: 8123,
        ..Default::default()
    };
    ui.set_form(ui_state::form_from_config(&config));
    ui.set_can_start(true);
    ui.set_is_running(true);
    ui.set_app_version(crate::api::app_version().into());
    ui.set_config_loading(false);

    let usage = UsageSummary {
        plan: "PRO".into(),
        total_limit: 1700.0,
        total_used: 400.0,
        percent: 24,
        reset_date: "2026-10-01".into(),
        has_overage_info: true,
        overage_enabled: true,
        ..Default::default()
    };
    ui.set_usage(ui_state::usage_view(&usage));

    let models = vec![
        ModelRow {
            id: "claude-sonnet-4-6".into(),
            rate_text: "1.30x credits".into(),
            description: "Claude Sonnet 4.6 model with 1M context window".into(),
            supports_thinking: true,
        },
        ModelRow {
            id: "claude-opus-4-1".into(),
            rate_text: "2.20x credits".into(),
            description: "Claude Opus 4.1 model with 1M context window".into(),
            supports_thinking: false,
        },
    ];
    ui.set_models(ModelRc::new(VecModel::from(models)));

    let processed = logs::process(&[
        "2026-02-10 18:11:11 | INFO | \u{1b}[32mstarted\u{1b}[0m".to_string(),
        "plain line".to_string(),
    ]);
    ui.set_logs(ModelRc::new(VecModel::from(ui_state::log_rows(&processed))));
    ui.set_logs_count_text("2 events captured".into());

    ui.set_detected_cli_dbs(ModelRc::new(VecModel::from(vec![SharedString::from(
        "/tmp/data.sqlite3",
    )])));
    ui.set_detected_creds_files(ModelRc::new(VecModel::from(vec![SharedString::from(
        "/tmp/creds.json",
    )])));

    ui.set_languages(ModelRc::new(VecModel::from(
        crate::i18n::LANGUAGES
            .iter()
            .map(|(_, label)| SharedString::from(*label))
            .collect::<Vec<_>>(),
    )));
    ui.set_example_code(
        crate::examples::render(
            ApiFlavor::OpenAi,
            Snippet::Curl,
            &config.server_host,
            config.server_port,
            &config.proxy_api_key,
        )
        .into(),
    );
    ui.set_example_code_display(
        crate::examples::render(
            ApiFlavor::OpenAi,
            Snippet::Curl,
            &config.server_host,
            config.server_port,
            &crate::examples::mask_key(&config.proxy_api_key),
        )
        .into(),
    );

    assert!(ui.get_usage().loaded && ui.get_usage().has_quota);
    assert_eq!(ui.get_logs().row_count(), 2);
    assert!(ui.get_example_code().contains("http://127.0.0.1:8123"));

    #[derive(Default)]
    struct Fired {
        names: Vec<&'static str>,
        example: Option<(i32, i32)>,
        language: Option<i32>,
        copied: Option<String>,
    }
    let fired = Rc::new(RefCell::new(Fired::default()));

    macro_rules! record {
        ($setter:ident, $name:literal) => {{
            let fired = Rc::clone(&fired);
            ui.$setter(move || fired.borrow_mut().names.push($name));
        }};
    }

    record!(on_start_server, "start_server");
    record!(on_stop_server, "stop_server");
    record!(on_restart_server, "restart_server");
    record!(on_form_changed, "form_changed");
    record!(on_save_config, "save_config");
    record!(on_generate_key, "generate_key");
    record!(on_restart_after_save, "restart_after_save");
    record!(on_refresh_usage, "refresh_usage");
    record!(on_clear_logs, "clear_logs");
    record!(on_export_logs, "export_logs");
    record!(on_copy_code, "copy_code");

    {
        let fired = Rc::clone(&fired);
        ui.on_example_changed(move |flavor, snippet| {
            fired.borrow_mut().example = Some((flavor, snippet));
        });
    }
    {
        let fired = Rc::clone(&fired);
        ui.on_select_language(move |index| fired.borrow_mut().language = Some(index));
    }
    {
        let fired = Rc::clone(&fired);
        ui.on_copy_text(move |text| fired.borrow_mut().copied = Some(text.to_string()));
    }

    ui.invoke_start_server();
    ui.invoke_stop_server();
    ui.invoke_restart_server();
    ui.invoke_form_changed();
    ui.invoke_save_config();
    ui.invoke_generate_key();
    ui.invoke_restart_after_save();
    ui.invoke_refresh_usage();
    ui.invoke_clear_logs();
    ui.invoke_export_logs();
    ui.invoke_copy_code();
    ui.invoke_example_changed(1, 2);
    ui.invoke_select_language(1);
    ui.invoke_copy_text("claude-sonnet-4-6".into());

    {
        let seen = fired.borrow();
        assert_eq!(
            seen.names.len(),
            11,
            "every plain callback must reach Rust: {:?}",
            seen.names
        );
        assert_eq!(
            seen.example,
            Some((1, 2)),
            "tab selection must carry indices"
        );
        assert_eq!(seen.language, Some(1));
        assert_eq!(seen.copied.as_deref(), Some("claude-sonnet-4-6"));
    }

    for view in [0, 1, 2, 3] {
        ui.set_current_view(view);
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(60));
        assert_eq!(ui.get_current_view(), view);
    }

    ui.set_current_view(0);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(60));

    let click = |id: &str| {
        let element = ElementHandle::find_by_element_id(&ui, id)
            .next()
            .unwrap_or_else(|| panic!("no element with id {id}"));
        assert!(
            element.size().width > 0.0 && element.size().height > 0.0,
            "{id} has no clickable area"
        );
        element.mock_single_click(PointerEventButton::Left);
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(60));
    };

    let before = fired.borrow().names.len();
    click("StatusCard::usage-refresh");
    click("MainWindow::server-switch");

    click("ApiExamplesView::copy-button");

    ui.set_current_view(1);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(60));
    let model_rows = ElementHandle::find_by_element_id(&ui, "ModelsView::model-row").count();
    assert_eq!(
        model_rows, 2,
        "models tab must render one row per model, got {model_rows}"
    );

    ui.set_current_view(0);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(60));

    let names = &fired.borrow().names[before..];
    assert!(
        names.contains(&"copy_code"),
        "clicking the copy control must reach Rust, got {names:?}"
    );
    assert!(
        names.contains(&"refresh_usage"),
        "clicking an IconButton must reach Rust, got {names:?}"
    );
    assert!(
        names.contains(&"stop_server"),
        "clicking the server switch must reach Rust, got {names:?}"
    );
    assert!(
        ui.get_is_running(),
        "the switch must not write its own checked state"
    );

    ui.set_config_loading(true);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(60));
    ui.set_config_loading(false);

    let status_card_width = || {
        ElementHandle::find_by_element_id(&ui, "StatusCard::card")
            .next()
            .expect("StatusCard::card must exist")
            .size()
            .width
    };
    ui.set_current_view(0);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(60));
    let running_width = status_card_width();
    assert!(running_width > 0.0, "status card must have a width");

    ui.set_is_running(false);
    ui.set_usage(ui_state::usage_placeholder(false, "HTTP 500"));
    ui.set_models(ModelRc::new(VecModel::<ModelRow>::default()));
    ui.set_logs(ModelRc::new(VecModel::<LogRow>::default()));
    ui.set_current_view(0);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(60));
    assert!(!ui.get_is_running());

    let stopped_width = status_card_width();
    assert_eq!(
        running_width, stopped_width,
        "status card must keep the same width when the gateway stops \
         (running: {running_width}, stopped: {stopped_width})"
    );
}
