> [English](../compatibility.md) | 简体中文

# 客户端兼容性（`compat`）

Kiro（以及其背后的 AWS control plane）有一些怪癖，它们既不属于 `convert`（结构转换），也不属于 `auth`（凭据），但几乎每个请求/响应都需要处理。`compat` 把这些处理收集为一个个小巧、可独立测试的 `RequestHook`/`ResponseHook` 实现，由 `CompatibilityPipeline` 按顺序执行。

```rust
pub trait RequestHook: Send + Sync {
    fn name(&self) -> &'static str;
    fn enabled(&self) -> bool { true }
    fn apply(&self, request: &mut CompatRequest) -> Result<()>;
}
```

`CompatibilityPipeline::new()` 会装配默认的 hook 集合：

| 顺序 | Hook | 作用 |
|---|---|---|
| 1 | `ToolNameAliasHook` | 占位符：实际逻辑位于 `ToolNameAliases`，由 `convert` 直接调用 |
| 2 | `ProfileArnAutofetchHook` | 占位符：实际逻辑由 `auth::AuthManager` 直接调用 |
| 3 | `ControlPlaneHostHook` | 把仅限 control plane 的路径改写到 AWS 主机 |
| 4 | `ChatHostFallbackHook` | 占位符：实际逻辑由 `auth::AuthManager::api_host` 直接调用 |
| — | `ModelIdFormatHook`（响应） | 为 Claude 品牌客户端改写 `/v1/models` 中的 id |

请求 hook 按注册顺序执行；响应 hook 按注册顺序的*逆序*执行，即最后注册的响应 hook 最先执行，这与请求管道从外到内层层包裹的直觉相对应。有几个 hook 在管道本身中只是空操作的占位符（它们的实际逻辑由 `convert`/`auth` 直接调用，因为那些调用点掌握着通用管道所没有的信息），但它们仍保留在这里，以便 `request_hook_names`/`response_hook_names` 能准确报告哪些兼容性行为处于启用状态，用于诊断。

## 工具名别名

Kiro 将 `toolSpecification.name` 限制为最多 64 个 ASCII 字母数字/`_`/`-` 字符（`MAX_KIRO_TOOL_NAME_LENGTH`），但 OpenAI/Anthropic 客户端经常发送更长或格式不同的名称。像 `mcp__plugin_everything_claude_code_github__create_pull_request_review` 这样的 MCP 风格工具名就是超出限制的常见情况。

`ToolNameAliases::needs_alias` 会标记任何为空、超过长度限制或包含合法字符集之外字节的名称。`alias_for` 以确定性的方式把这样的名称映射为一个简短且避免冲突的别名（在同一次请求/响应周期内，同一名称复用同一别名），`original_for` 则执行反向映射。

每个请求都会新建一个 `ToolNameAliases` 实例（由 `convert::openai`/`convert::anthropic` 创建，且在历史转换*之前*，参见 [conversion.md](conversion.md)），这样在旧的 assistant 轮次中引用的工具和新的工具定义中同名的工具会得到相同的别名。还原发生在返回的路上，共有三处：

- `original_for`：直接还原 tool call 的 `name` 字段。
- `restore_text`：还原嵌入在累积（非流式）文本中的 `[Called <alias> with args:...]` 形式的方括号标记。
- `restore_text_fragment`：流式安全的变体。每次调用都会保留足够多的尾部文本，避免标记被切分到不同的 chunk 边界两侧；只有在确定没有标记跨越切分点时才输出文本。

它在响应管道中的调用位置参见 [streaming.md](streaming.md#工具名还原)。

## 主机改写

Kiro 暴露了两个概念上不同的主机。它们目前都解析为同一个 `runtime.<region>.kiro.dev` 模板，但仍被区分开，因为二者将来可能分化，而且在不同操作中选择二者的*规则*也不同：

- **Control-plane 主机**（`KIRO_Q_HOST_TEMPLATE`）：用于模型列表、用量限制、MCP 相关的控制操作。这些操作始终无条件地使用它，通过 `compat::control_plane_host` / `ControlPlaneHostHook` 实现。
- **Chat/runtime 主机**（`KIRO_API_HOST_TEMPLATE`）：用于 `generateAssistantResponse` 的付费 runtime。它要求 profile ARN；没有 profile ARN 的账户（例如 Builder ID / 免费账户）在聊天时也会被路由到 control-plane 主机，因为付费 runtime 主机会拒绝它们。这个回退由 `compat::chat_host` 实现，并由 `auth::AuthManager::api_host` 直接调用。

## 模型 ID 格式化

Claude Code 以及类似的 Claude 品牌客户端期望模型 ID 使用短横线（`claude-sonnet-4-6`），而不是 Kiro 的 `/v1/models` 端点实际返回的点号形式（`claude-sonnet-4.6`）。`ModelIdFormatHook`（响应 hook）及其辅助函数 `rewrite_model_ids`/`is_claude_client` 会把 `GET /v1/models` 的响应改写为短横线形式，但*仅*在请求看起来来自 Claude 品牌客户端时才这样做，其他客户端看到的是未经修改的 Kiro id。这一层以 `server.rs` 中的 `server::model_id_format_middleware` 的形式叠加，因此无论响应由哪个路由器（OpenAI 形式或 Anthropic 形式的 `/v1/models`）提供，都会统一生效。

## `CompatRequest` / `CompatResponse`

hook 所操作的可变请求/响应视图：

```rust
pub struct CompatRequest {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
    pub upstream_url: Option<String>,  // set by a hook to redirect the request
}

pub struct CompatResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}
```

二者都是开销很低、自包含的结构体，因此无需构造真实的 HTTP 请求/响应就能对 hook 进行单元测试。
