# Desktop App (`lanius-gui`)

`lanius-desktop` is a Slint-based desktop wrapper that runs the
`lanius-core` gateway **in-process** (not as a separate child process) and
adds a UI for configuration, live logs, model listing, and usage. It
contains no protocol logic of its own — every request/response concern
lives in `lanius-core`; this crate is purely about presenting and
controlling that engine.

## Module map

```
main.rs          entry point: window/runtime bootstrap, tray event pump,
                 Slint callback wiring (delegates to controller.rs)
controller.rs    Controller — the hub tying everything below together
server.rs        ServerManager — embedded gateway lifecycle (start/stop),
                 AppConfig -> lanius_core::Config translation
config.rs        AppConfig — persisted GUI settings (superset of what
                 lanius_core::Config needs, e.g. auth method selection)
api.rs           HTTP client for the *running* gateway's own API
                 (usage, models, health) — talks to itself over loopback
process.rs       port-conflict detection/resolution before (re)starting
i18n.rs / tr_generated.rs   translated strings (Slint Tr global + tray)
log_capture.rs   tracing Layer that feeds the GUI's log view
logs.rs          log line post-processing (ANSI stripping, timestamp split)
tray.rs          native tray icon/menu
examples.rs      generated code-snippet examples shown in the UI
autostart.rs     OS-level "launch at login" registration
macos.rs         macOS-specific Dock visibility toggling (cfg-gated)
ui/              Slint UI definitions (.slint files)
```

## `Controller` — the hub

`Controller` (in `controller.rs`) is the single object wiring UI state, the
embedded gateway, i18n, the tray, and background maintenance tasks
together. `main.rs` creates exactly one instance, wrapped in `Arc` since
it's shared across every Slint callback and every spawned background task.
Every public async method on `Controller` is meant to be spawned as its own
Tokio task from a Slint callback; none assume they run on a particular
thread, but all UI mutation is marshaled onto the Slint UI thread via
`Controller::with_ui` (which calls Slint's `upgrade_in_event_loop`).

From `Controller`'s perspective:
- **`server.rs`'s `ServerManager`** is the embedded gateway's lifecycle,
  owned behind `self.server: Mutex<ServerManager>`.
- **`config.rs`** is the persisted settings this controller reads/writes.
- **`api.rs`** is how it talks to the *running* gateway's own HTTP API
  (usage, models, health) once started — i.e. the GUI is also a client of
  its own embedded gateway.
- **`process.rs`** detects/resolves port conflicts before (re)starting.
- **`tray.rs`** is driven indirectly: `Controller` never touches a `Tray`
  handle directly (it doesn't live on this thread); it stages updates into
  `self.tray: Arc<StdMutex<TrayShared>>`, which `main.rs`'s tray timer
  callback drains and applies. `TrayShared`'s fields are one-shot requests
  (`Option::take`/`mem::take` on apply), so `Controller` never needs the
  actual tray handle to communicate with the tray thread.
- **`ui_state.rs`** converts between `Controller`'s domain types
  (`AppConfig`, `UsageSummary`, processed log lines) and Slint's generated
  struct types.

## `ServerManager` — embedded gateway lifecycle

`ServerManager` (in `server.rs`) wraps `lanius_core::server::spawn`/
`GatewayHandle` (see `crates/lanius-core/src/server.rs`) to start/stop the
gateway and tracks a `ServerStatus` (`"stopped"`/`"starting"`/`"running"`/
`"error"`, plus the bound port and any error message) surfaced to the UI.

`build_gateway_config` is the translation layer from the GUI's own
`AppConfig` (which additionally models things like "which auth method is
selected" that `lanius_core::Config` doesn't need to know about directly)
into the actual `lanius_core::Config` the core gateway understands.

Status/log messages produced here go through the same shared `LogBuffer`
as `log_capture.rs`'s `tracing`-based capture, so gateway lifecycle events
(started, stopped, port conflict, etc.) appear in the GUI's log view
alongside ordinary `tracing::info!`/`warn!`/`error!` log lines from the
running gateway.

## UI (Slint)

`main.rs` calls `slint::include_modules!()`, which pulls in the compiled
Slint UI definitions from `ui/*.slint` (`MainWindow`, `ConfigForm`, `Tr`,
and other generated types referenced throughout the crate). This requires
the Slint build script (`build.rs`) to have run successfully, which in turn
requires the platform's GUI toolkit dependencies to be present at build
time — see the root [README.md](../README.md#building) for the practical
build requirements.

`ui/views/` holds one `.slint` file per major UI section (settings, models,
sidebar, common widgets); `ui/generated/tr.slint` is the generated
translation table consumed alongside `tr_generated.rs`.

## i18n

`i18n.rs`/`tr_generated.rs` supply translated strings pushed both to the
Slint `Tr` global (bound into the UI) and to the tray menu labels.
`crates/lanius-gui/i18n/{en,zh}.json` are the source translation files;
`tr_generated.rs` is generated from them (see the crate's build tooling for
the generation step) and must stay in sync — every key present in one
locale must be present in `en.json` at minimum, since English is the
fallback for missing translations.

## Testing

`ui_tests.rs` (behind `#[cfg(test)]`, registered in `main.rs`) covers the
non-Slint-dependent logic in this crate: config mapping, log processing,
process/port detection, and the usage/model API response parsing in
`api.rs`. `crates/lanius-cli/tests/gui_api_contract.rs` is a
cross-crate integration test asserting that `lanius-gui`'s config mapping
and gateway spawn/handle usage stay compatible with what `lanius_core`
actually expects — it exists specifically to catch drift between the two
crates without needing to build the full Slint UI.
