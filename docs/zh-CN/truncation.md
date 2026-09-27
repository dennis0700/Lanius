> [English](../truncation.md) | 简体中文

# 截断检测与恢复（`truncation`）

Kiro 施加的输出大小限制可能会在流中途截断响应：tool call 的 JSON 参数以不平衡的状态结束，或者纯文本在没有正常结束的情况下戛然而止。如果不加处理，要么会把格式错误的 JSON 暴露给客户端，要么会悄无声息地返回不完整的输出，而没有任何出错提示。`truncation` 负责检测这种情况，并在同一对话的*下一*轮中注入一条通知，让模型明白这次截断是 API 的限制而非它自己的错误，从而调整做法，而不是重复同样（很可能再次被截断）的调用。

## 检测截断

有两个相互独立的信号，在不同的层次上检查：

- **Tool call 截断**：在解析时诊断（参见 `upstream::parser::diagnose_json_truncation` 和 [streaming.md](streaming.md)）。如果某个 tool call 累积的 JSON 参数始终无法被干净地解析，解析器会解释原因（例如 "missing 2 closing brace(s)"），并将 `arguments` 替换为 `"{}"`，诊断结果记录在 `ToolCall::truncation` 上。
- **纯内容截断**：`truncation::is_content_truncated`，一个在流结束后检查的启发式判断。当流*没有*正常结束、确实产生了一些内容、且这些内容不是 tool call 时（tool call 截断如上所述单独诊断），结果为 true。

两个响应构建器（`api::openai::sse::response_value_from_result` 和 `api::anthropic::sse::response_from_stream_result`）都会在收集完整响应后立即检查这些信号，并调用 `TruncationStore::save_*` 记录发生的情况，以对话 id 为键。

## `TruncationStore`

一个线程安全（`Arc<Mutex<_>>`）、受 TTL 和容量约束的缓存（`DEFAULT_TRUNCATION_TTL` 为 30 分钟，`DEFAULT_TRUNCATION_CACHE_CAPACITY` 为 1024 条，超出容量时按 LRU 淘汰），保存两类记录：

- **`ToolTruncationInfo`**：以 `(conversation_id, tool_call_id)` 为键；包含工具名和原始诊断 JSON。
- **`ContentTruncationInfo`**：以 `(conversation_id, message_hash)` 为键，其中哈希基于被截断内容的前 500 个字符计算；包含一个 200 字符的预览。

条目**恰好只会被消费一次**：每个 `take_*` 方法在返回条目的同时将其移除，而 `get_*` 方法只是具有相同一次性语义的薄别名（仅为调用点的可读性而保留，例如在调用点区分"检查"与"消费"的意图，尽管行为完全相同）。这意味着每次截断事件最多只会注入一次恢复通知：如果客户端用相同的历史重试，不会因为已经看到过的截断而再次收到通知。

## 恢复注入

在同一对话中客户端的*下一次*请求时，路由处理器（`api::openai::routes::inject_truncation_recovery`、`api::anthropic::routes::apply_truncation_recovery`）会在存储中查找与该对话匹配的待处理记录，若找到：

- **Tool call 截断** → `prepend_tool_recovery_notice` 会在原始工具结果文本发回给模型之前，在其前面加上 `TRUNCATION_TOOL_RESULT_MESSAGE`（一条固定的说明性通知），这样模型既能看到解释，也能看到工具实际返回的内容。对于没有真实工具结果可供前置的情况，`generate_truncation_tool_result` 会改为构建一个合成的、带错误标记的 `tool_result` 块。
- **纯内容截断** → `generate_truncation_user_message` 返回 `TRUNCATION_USER_MESSAGE`，这是一条固定的系统通知字符串，告知模型它上一次的响应是被 API 截断的，而不是它自己出了错。

整个机制由 `config.truncation_recovery` 控制（默认启用），并且只会向已有的轮次*追加*说明性文本：它从不伪造带有看似真实数据的工具结果，也从不阻塞或自行重试请求。

## 为什么它位于 `convert`/`upstream` 之外

截断横跨两个层次（tool call 在解析器层面诊断，文本在流结束层面进行启发式判断），并且会*跨*请求持久化状态（通知是在被截断的那次请求*之后*的请求中注入的）。它既不适合放进 `convert`（无状态、单请求的结构转换），也不适合放进 `upstream`（没有跨请求的状态）。把它集中在这里，既能让这两个模块保持简单，也能把恢复提示文本和缓存淘汰策略放在同一处。
