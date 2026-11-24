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

## Kiro 主机与客户端身份

Lanius 与 Kiro 的交互方式和 Kiro CLI 3.5.0 的默认（V3）agent 相同：使用同样两个服务主机（都在 Kiro 的[防火墙白名单](https://kiro.dev/docs/privacy-and-security/firewalls/)上），请求形态以 `kiro-cli` 3.5.0 的真实流量为准。

- **Runtime 主机**（`KIRO_API_HOST_TEMPLATE`，`runtime.<region>.kiro.dev`）：`KiroRuntimeService.GenerateAssistantResponse` 和 `KiroRuntimeService.InvokeMCP`。
- **Management 主机**（`KIRO_MANAGEMENT_HOST_TEMPLATE`，`management.<region>.kiro.dev`）：`KiroControlPlaneBearerService` 的 `ListAvailableProfiles`、`ListAvailableModels`、`GetUsageLimits` 和 `GetProfile`。参数只放在 JSON 请求体中（Kiro CLI 2.x 还会在 query string 里重复一遍）。

所有请求都以交互式 Kiro CLI 的 agent harness（KAS 0.66.119，基于 AWS SDK for JavaScript）身份发出，例如：

```
User-Agent: aws-sdk-js/1.0.0 ua/2.1 os/linux#<内核版本> lang/js md/nodejs#22.23.4
  api/kiroruntime#1.0.0 m/N KiroCLI/3.5.0 KAS/0.66.119 os/linux md/appVersion-3.5.0
  app/AmazonQ-For-CLI acp-client/kiro-cli
x-amz-user-agent: aws-sdk-js/1.0.0 KiroCLI/3.5.0 KAS/0.66.119 os/linux
  md/appVersion-3.5.0 app/AmazonQ-For-CLI acp-client/kiro-cli
```

`os/` 使用 Node.js 的平台名（`darwin`、`linux`、`win32`）。聊天请求额外带 `x-amzn-kiro-client-attribution: kiro-cli`、`x-amzn-kiro-mode-identifier: default-v2`、`x-amzn-kiro-fallback: 0;max=0` 和 `x-amzn-kiro-attempt: 1;max=3`。`TokenType` 对 API key 为 `API_KEY`，对 IAM Identity Center 登录为 `SSO_OIDC`，社交登录不发送。Lanius 继续发送 `x-amzn-codewhisperer-optout: true`，即 Kiro CLI 在用户未开启内容收集时发送的值。

聊天请求体同样与 Kiro CLI 一致：`conversationState` 每个请求带一个新的 `agentContinuationId`，并以会话自身作为 `rootConversationId`；请求顶层带 `agentMode: "default-v2"`；不发送 `envState`；system prompt 作为 user 轮放在 history 开头，后接 `I will follow these instructions.`（除非服务端开关允许，否则 Kiro 会以 `REQUEST_BODY_INVALID` 拒绝 `systemPrompt` 字段）。

Kiro 已把旧的 `q.<region>.amazonaws.com` 主机标为 legacy（Kiro CLI 现在只往那里发遥测），所以 Lanius 不再使用它。客户端不做端点轮换，重试和退避都发往同一个主机。

## 原生 reasoning 回传

Kiro 会为每个原生 reasoning 块签名，并在它被发回时校验签名。Lanius 只在 reasoning 由 Kiro 签发时，才把 assistant 轮的 reasoning 作为 `reasoningContent` 回传：签名不是 Lanius 生成的 Anthropic `thinking` 块（Lanius 自己的占位签名以 `sig_` 开头），或者 `redacted_thinking` 块（Lanius 由 Kiro 的 `redactedContent` 生成）。其他签名会被 Kiro 以 `THINKING_SIGNATURE_INVALID` 拒绝，因此带占位签名的块会被丢弃。服务端 `web_search` 的每一轮也以同样方式回传该轮的 reasoning。

Kiro 还会报告模型停止的原因（`metadataEvent.stopReason`）。`MAX_TOKENS` 表示回答被截断（`finish_reason: "length"`、`stop_reason: "max_tokens"`、Responses 的 `incomplete`）；没有停止原因时，Lanius 退回到基于流是否正常结束的判断。

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
