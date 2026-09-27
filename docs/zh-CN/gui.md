> [English](../gui.md) | 简体中文

# 桌面应用（`lanius-gui`）

`lanius-desktop` 是基于 Slint 的桌面封装，它在**进程内**运行 `lanius-core` 网关（而不是作为单独的子进程），并提供用于配置、实时日志、模型列表和用量查看的 UI。它本身不包含任何协议逻辑，所有请求/响应相关的处理都在 `lanius-core` 中；这个 crate 只负责展示和控制该引擎。

## 模块一览

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

## `Controller`：中枢

`Controller`（位于 `controller.rs`）是将 UI 状态、嵌入式网关、i18n、托盘以及后台维护任务连接在一起的唯一对象。`main.rs` 只创建一个实例，并用 `Arc` 包装，因为它会在每个 Slint 回调和每个派生的后台任务之间共享。`Controller` 上的每个公共异步方法都设计为从 Slint 回调中作为独立的 Tokio 任务派生；它们都不假定自己运行在特定线程上，但所有 UI 修改都会通过 `Controller::with_ui`（调用 Slint 的 `upgrade_in_event_loop`）转交到 Slint UI 线程上执行。

从 `Controller` 的角度来看：
- **`server.rs` 的 `ServerManager`** 负责嵌入式网关的生命周期，由 `self.server: Mutex<ServerManager>` 持有。
- **`config.rs`** 是该 controller 读写的持久化设置。
- **`api.rs`** 是网关启动后，controller 与*正在运行*的网关自身 HTTP API（用量、模型、健康检查）通信的方式。也就是说，GUI 同时也是其嵌入式网关的客户端。
- **`process.rs`** 在（重新）启动前检测并解决端口冲突。
- **`tray.rs`** 是间接驱动的：`Controller` 从不直接操作 `Tray` 句柄（它不在这个线程上）；而是把更新暂存到 `self.tray: Arc<StdMutex<TrayShared>>` 中，由 `main.rs` 的托盘定时器回调取出并应用。`TrayShared` 的字段都是一次性请求（应用时使用 `Option::take`/`mem::take`），因此 `Controller` 无需持有真正的托盘句柄即可与托盘线程通信。
- **`ui_state.rs`** 负责在 `Controller` 的领域类型（`AppConfig`、`UsageSummary`、处理后的日志行）与 Slint 生成的结构体类型之间进行转换。

## `ServerManager`：嵌入式网关生命周期

`ServerManager`（位于 `server.rs`）封装了 `lanius_core::server::spawn`/`GatewayHandle`（参见 `crates/lanius-core/src/server.rs`），用于启动/停止网关，并跟踪展示给 UI 的 `ServerStatus`（`"stopped"`/`"starting"`/`"running"`/`"error"`，以及绑定的端口和可能的错误信息）。

`build_gateway_config` 是转换层，负责把 GUI 自身的 `AppConfig`（它还额外建模了诸如“选择了哪种认证方式”这类 `lanius_core::Config` 无需直接了解的内容）转换为核心网关能够理解的实际 `lanius_core::Config`。

这里产生的状态/日志消息与 `log_capture.rs` 基于 `tracing` 的捕获共用同一个 `LogBuffer`，因此网关生命周期事件（已启动、已停止、端口冲突等）会与运行中网关产生的普通 `tracing::info!`/`warn!`/`error!` 日志行一起显示在 GUI 的日志视图中。

## UI（Slint）

`main.rs` 调用 `slint::include_modules!()`，引入由 `ui/*.slint` 编译而来的 Slint UI 定义（`MainWindow`、`ConfigForm`、`Tr` 以及整个 crate 中引用的其他生成类型）。这要求 Slint 构建脚本（`build.rs`）成功运行，而这又要求构建时存在平台的 GUI 工具包依赖。实际的构建要求请参见根目录的 [README.md](../../README.zh-CN.md#构建)。

`ui/views/` 中每个主要 UI 区域（设置、模型、侧边栏、通用控件）各有一个 `.slint` 文件；`ui/generated/tr.slint` 是生成的翻译表，与 `tr_generated.rs` 配合使用。

## i18n

`i18n.rs`/`tr_generated.rs` 提供翻译后的字符串，同时推送到 Slint 的 `Tr` 全局对象（绑定到 UI 中）和托盘菜单标签。`crates/lanius-gui/i18n/{en,zh}.json` 是翻译源文件；`tr_generated.rs` 由它们生成（生成步骤见该 crate 的构建工具），且必须保持同步。任一语言中存在的每个键，至少都必须在 `en.json` 中存在，因为英文是缺失翻译时的回退语言。

## 测试

`ui_tests.rs`（位于 `#[cfg(test)]` 之后，在 `main.rs` 中注册）覆盖了该 crate 中不依赖 Slint 的逻辑：配置映射、日志处理、进程/端口检测，以及 `api.rs` 中对用量/模型 API 响应的解析。`crates/lanius-cli/tests/gui_api_contract.rs` 是一个跨 crate 的集成测试，断言 `lanius-gui` 的配置映射以及对网关 spawn/handle 的使用方式与 `lanius_core` 实际期望的保持兼容。它专门用于在无需构建完整 Slint UI 的情况下，捕获两个 crate 之间的偏差。
