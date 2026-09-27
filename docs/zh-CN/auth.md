> [English](../auth.md) | 简体中文

# 认证（`auth`）

`auth` 是网关关于“我们此刻如何向 Kiro/AWS 证明自己的身份”的唯一可信来源。它由三部分组成：

- **`auth::credentials`**：从所配置的来源加载原始凭据。
- **`auth::refresh`**：执行网络调用，用 refresh token 换取新的 access token。
- **`auth::AuthManager`**（位于 `auth.rs` 本身）：持有共享的、可变的 token 状态，决定何时刷新，在外部轮换 token 时进行重试，并将刷新后的凭据持久化回磁盘。

通常每个运行中的网关进程只有一个 `AuthManager`，通过 `Arc` 在 `AppState`/`OpenAiState`/`AnthropicState` 中共享。

## 凭据来源与优先级

`Credentials::load`（由 `AuthManager::new` 调用一次）按以下顺序合并各来源，后面的来源在提供了值的情况下会覆盖前面的来源：

1. **环境变量 / 配置**：`REFRESH_TOKEN` 和 `PROFILE_ARN` 提供初始值。
2. **`kiro-cli` SQLite 数据库**（`KIRO_CLI_DB_FILE`）：如果已配置，其优先级高于 JSON 凭据文件。`SQLITE_TOKEN_KEYS` 定义了在哪一条 `auth_kv` 记录中查找当前有效 token 的回退顺序（社交登录 > OIDC 设备注册）；如果使用的是 AWS SSO 流程，配套的 `SQLITE_REGISTRATION_KEYS` 查找会找到 OIDC client id/secret。
3. **JSON 凭据文件**（`KIRO_CREDS_FILE`）：仅在未配置 SQLite 数据库时才会读取。当凭据文件的 `clientIdHash` 字段指向企业设备注册时，`merge_enterprise_registration` 还会额外读取一个单独的 `~/.aws/sso/cache/*.json` 文件。

`Credentials` 中的所有字段都是 `Option`，因为没有任何单一来源能填充全部字段（仅使用 refresh token 的配置根本没有 `client_id`/`client_secret`）。

## 认证类型

`AuthType` 是推导出来的，从不直接配置：

- `AuthType::AwsSsoOidc`：`client_id` 和 `client_secret` 同时存在且非空（IAM Identity Center / 企业账户）。
- `AuthType::KiroDesktop`：其他情况（Kiro 桌面应用自己的刷新流程：只有 refresh token，没有 client 凭据）。

## Token 生命周期

`AuthManager::access_token`（几乎所有请求路径都会调用的方法）执行以下步骤：

1. **快速路径**：如果内存中的 token 距离过期还超过 `TOKEN_REFRESH_THRESHOLD`（10 分钟），则直接返回，不进行任何 I/O。
2. **SQLite 预查**：如果凭据来自 SQLite，且 token 看起来即将过期，则先从 SQLite 重新加载。另一个进程（`kiro-cli` 本身）可能已经刷新并写入了更新的 token，这样可以省去一次网络往返，并避免与该写入方产生竞争。
3. **网络刷新**：否则通过 `refresh::refresh` 执行实际的刷新，它会根据 `AuthType` 分派到桌面流程或 OIDC 流程。

`access_token_and_autofetch` 在此基础上进行包装，利用刚获取的 token 执行一次尽力而为、至多一次的 profile ARN 发现（见下文）；该步骤的失败会被记录日志后吞掉，绝不会作为错误抛出，因为它只是一项优化，而非硬性要求。

`force_refresh` 完全绕过“是否仍然有效”的快速路径：适用于已经知道缓存的 token 被上游拒绝（例如收到 401）、需要一个确保是新的 token 的调用方。

### SQLite HTTP-400 恢复

如果刷新的网络调用返回 HTTP 400，*并且*凭据来源是 SQLite，`AuthManager` 会假定另一个进程（例如 `kiro-cli login`）可能刚刚在它不知情的情况下轮换了 refresh token。在放弃之前，它会从 SQLite 重新加载并重试一次（`refresh_locked`/`should_reload_oidc_after_http_400`）；如果仍然无法得到 token，但从 SQLite 加载的 token 恰好尚未过期，则直接使用该 token，而不是因一个已经过时的 HTTP 400 而硬性失败。

## 刷新流程（`auth::refresh`）

有两种流程，由 `AuthType` 选择：

- **Kiro Desktop**：`POST {desktop_url}`，请求体为 `{"refreshToken": ...}`，刻意保持最简，不含 client 凭据或 scope。
- **AWS SSO OIDC**：`POST {oidc_url}`，请求体为 OIDC `refresh_token` grant 的内容（`grantType`、`clientId`、`clientSecret`、`refreshToken`），刻意省略任何 `scope`/`scopes` 字段，因为这里使用的端点会拒绝包含该字段的请求。

两种流程都只会在 `AuthManager` 持有其状态锁时被调用，因此每个 manager 同一时间最多只有一个刷新在进行：并发的调用方只会等待同一把锁，而不会触发多余的刷新。

**安全不变式**：`refresh.rs` 中的每条错误路径都只暴露 HTTP 状态码或固定的、不带参数的消息，绝不暴露原始响应体，因为该响应体完全可能包含刚签发的（或即将被取代的）access/refresh token。`auth.rs` 自己的错误构造函数（例如 `sqlite_refresh_failed_error`）也遵循同样的原则。

## 持久化

刷新成功后，`AuthManager::apply_outcome` 更新内存中的状态，然后 `persist` 将更新后的凭据写回所配置的存储（SQLite 优先于 JSON 文件；如果两者都未配置，则不做任何操作）：

- **`persist_file`**：以原子方式重写 JSON 凭据文件（保留所有无法识别的已有字段），因此崩溃或并发读取永远不会看到写了一半的文件。
- **`persist_sqlite`**：写回匹配的 `auth_kv` 记录，优先使用凭据最初加载时所用的 key，然后按 `SQLITE_TOKEN_KEYS` 依次回退。遵循 `SQLITE_READONLY=true`（跳过写入，并以 debug 级别记录日志），并将数据库文件缺失或无法打开视为软失败：即使磁盘上的副本未能更新，内存中的 token 也已经更新，当前进程可以继续使用。

持久化失败总是会被记录日志，但绝不会作为错误传递给正在进行的请求：写回失败不应阻止返回一个完全有效、刚刚刷新过的 token。

## Profile ARN 自动获取

Kiro 的付费 “runtime” host 需要 profile ARN；没有 profile ARN 的 Builder ID 账户，其聊天请求也会回退到（免费的）control-plane host（见 [compatibility.md](compatibility.md#主机改写)）。如果没有配置 profile ARN，`AuthManager::autofetch_profile_arn` 会尝试通过调用 Kiro 的 `ListAvailableProfiles` control-plane API 来发现一个，每个进程生命周期内最多尝试一次（由 `compat::ProfileArnAutofetchHook::claim_fetch` 以原子方式占用这次尝试）。任何失败（网络错误、非 200、JSON 格式错误、响应中没有可用的 profile）都会以 `warn` 级别记录日志，除此之外被忽略。

## 区域解析

实际生效的 API 区域（`AuthManager::region`）按以下优先顺序确定：从 profile ARN 中检测到的区域，其次是随凭据一同报告的 SSO 区域，最后是静态配置的 `KIRO_REGION` 默认值（`final_api_region`）。

<a id="retry-and-refresh-policy"></a>

## 重试与刷新策略（上游客户端）

`upstream::client::KiroHttpClient` 在本模块之上叠加了自己的重试策略：HTTP 403 会在重试前触发强制 token 刷新（`AuthManager::force_refresh`）；HTTP 429/5xx 会以指数退避重试（`BASE_RETRY_DELAY * 2^attempt`，上限为 `MAX_RETRIES`）；传输层失败只有在 `error::classify_network_error` 将其标记为可重试时才会重试。它在整体请求流程中的位置见 [architecture.md](architecture.md)。
