> [English](../configuration.md) | 简体中文

# 配置

Lanius 完全通过环境变量进行配置（如果工作目录中存在 `.env` 文件，也会通过 `dotenvy` 自动加载）。`Config::from_env()` 在启动时读取一次环境变量，对于未设置或无法解析的项，回退到 `Config::default()`（无法解析的值会记录一条警告，而不会直接失败）。随后，`Config::validate()` 会在服务器开始处理流量之前运行一次，如果配置不安全或无效则拒绝启动。

`crates/lanius-core/src/config.rs` 是所有默认值的唯一事实来源；下表与其保持一致。

## 服务器

| 变量 | 默认值 | 用途 |
|---|---|---|
| `SERVER_HOST` | `0.0.0.0` | 网关 HTTP 服务器的绑定地址。 |
| `SERVER_PORT` | `8000` | 绑定端口。 |
| `PROXY_API_KEY` | *(不安全的占位值，必须设置)* | 客户端访问 Lanius 时必须提供的 Bearer 密钥。为空或仅含空白字符时校验失败。 |

## Kiro / AWS 连接

| 变量 | 默认值 | 用途 |
|---|---|---|
| `KIRO_REGION` | `us-east-1` | 用于构建 Kiro/OIDC 端点 URL 模板的 AWS 区域。为空时校验失败。 |
| `VPN_PROXY_URL` | 未设置 | 可选的 HTTP/HTTPS/SOCKS5 代理，用于所有上游 Kiro 请求。必须以 `http://`、`https://`、`socks5://` 或 `socks5h://` 开头。 |

## 凭据

| 变量 | 默认值 | 用途 |
|---|---|---|
| `REFRESH_TOKEN` | 未设置 | 用于向 Kiro 认证的 refresh token（作为 `Credentials` 的初始值；可能被文件/SQLite 来源覆盖）。 |
| `PROFILE_ARN` | 未设置 | 与该 refresh token 关联的可选 AWS IAM profile ARN。 |
| `KIRO_CREDS_FILE` | 未设置 | Kiro 桌面版 JSON 凭据文件的路径（会展开 `~`）。 |
| `KIRO_CLI_DB_FILE` | 未设置 | Kiro CLI 的 SQLite 数据库路径，这是另一种凭据来源，两者同时设置时优先于 `KIRO_CREDS_FILE`。 |
| `SQLITE_READONLY` | `false` | 为 `true` 时，刷新后的凭据永远不会写回 SQLite 数据库。 |

这些来源如何合并及确定优先级，参见 [auth.md](auth.md)。

## 请求处理

| 变量 | 默认值 | 用途 |
|---|---|---|
| `TOOL_DESCRIPTION_MAX_LENGTH` | `10000` | 工具描述的最大长度，超过后会改为移入系统提示词（参见 [conversion.md](conversion.md)）。 |
| `TRUNCATION_RECOVERY` | `true` | 工具/内容输出被截断时是否触发恢复提示流程（参见 [truncation.md](truncation.md)）。 |
| `KIRO_MAX_PAYLOAD_BYTES` | `600000` | 发送到上游的请求 payload 最大大小，单位为字节（按 Kiro 自身的计算方式）。 |
| `AUTO_TRIM_PAYLOAD` | `false` | 为 `true` 时，超大的 payload 会被自动裁剪（丢弃最早的历史记录），而不是被拒绝。 |
| `WEB_SEARCH_ENABLED` | `true` | 是否为 OpenAI 风格的请求声明/注入合成的 `web_search` 工具。 |

## 超时与重试

| 变量 | 默认值 | 用途 |
|---|---|---|
| `FIRST_TOKEN_TIMEOUT` | `15`（秒，浮点数） | 等待流式响应第一个字节的最长时间，超时即失败。 |
| `STREAMING_READ_TIMEOUT` | `300`（秒，浮点数） | 后续分块之间的最长等待时间，超时则判定流中途停滞并失败。 |
| `FIRST_TOKEN_MAX_RETRIES` | `3` | 等待第一个流式 token 时的最大尝试次数。 |

非流式请求还额外使用固定的 300 秒总超时和 30 秒连接超时（无法通过环境变量配置；参见 `upstream::client::NON_STREAM_REQUEST_TIMEOUT`/`CONNECT_TIMEOUT`）。`MAX_RETRIES`（3）和 `BASE_RETRY_DELAY`（1 秒，指数退避）控制 403/429/5xx 的重试策略，目前同样无法通过环境变量配置。

## 日志与调试

| 变量 | 默认值 | 用途 |
|---|---|---|
| `LOG_LEVEL` | `INFO` | `tracing` 日志详细程度（自动转为大写）。如果设置了 `RUST_LOG`，则以其为准。 |
| `DEBUG_MODE` | `off` | `off` \| `errors` \| `all`：额外的内部调试日志/转储。不区分大小写；无法识别的值回退为 `off`。 |
| `DEBUG_DIR` | `debug_logs` | 启用 `DEBUG_MODE` 时调试转储的写入目录。 |

## 模型别名与隐藏

`model_aliases` 和 `hidden_from_list` 目前不是环境变量，它们分别由 `config::default_model_aliases()`（`auto-kiro` -> `auto`）和 `config::default_hidden_from_list()`（`["auto"]`）填充。它们的用法参见 [model-catalog.md](model-catalog.md)。

## 校验规则

在以下情况下，`Config::validate()` 会使启动失败（返回 `GatewayError::Config`）：

- `PROXY_API_KEY` 为空或仅含空白字符，否则客户端可以使用任意密钥通过认证。
- `KIRO_REGION` 为空或仅含空白字符。
- `SERVER_PORT` 为 `0`。
- 设置了 `VPN_PROXY_URL`，但它不以四种可接受的协议之一开头。

## URL 模板

每个 Kiro/AWS 端点 URL 都由 `config.rs` 中带 `{region}` 模板的常量构建：

| 常量 | 模板 | 用途 |
|---|---|---|
| `KIRO_REFRESH_URL_TEMPLATE` | `https://prod.{region}.auth.desktop.kiro.dev/refreshToken` | Kiro 桌面版 token 刷新 |
| `AWS_SSO_OIDC_URL_TEMPLATE` | `https://oidc.{region}.amazonaws.com/token` | AWS SSO OIDC token 刷新 |
| `KIRO_API_HOST_TEMPLATE` | `https://runtime.{region}.kiro.dev` | 聊天/补全请求（付费 runtime） |
| `KIRO_Q_HOST_TEMPLATE` | `https://runtime.{region}.kiro.dev` | 模型列表、用量/账户查询（控制平面） |

它们通过 `Config::refresh_url()`、`Config::oidc_url()`、`Config::api_host()`、`Config::q_host()`、`Config::generate_assistant_response_url()` 和 `Config::list_available_models_url()` 对外提供。何时使用控制平面主机而非 runtime 主机的规则，参见 [compatibility.md](compatibility.md#主机改写)。
