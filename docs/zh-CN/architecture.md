> [English](../architecture.md) | 简体中文

# 架构

## Workspace 布局

Lanius 是一个包含三个 crate 的 Cargo workspace：

| Crate | 类型 | 职责 |
|---|---|---|
| `lanius-core` | library | 网关本体：HTTP 路由、协议转换、认证、流式解析器、模型目录以及全部业务逻辑。被下面两个二进制程序共同使用。 |
| `lanius-cli` | binary (`lanius`) | 无界面的服务进程，另外提供 `probe`/`replay` 调试子命令。只是一层薄封装，自身不包含任何协议逻辑。 |
| `lanius-gui` | binary (`lanius-desktop`) | 桌面应用（Slint UI），在进程内运行网关，并提供配置、实时日志、模型列表和用量视图。 |

`lanius-core` 被刻意设计为唯一理解 OpenAI/Anthropic/Kiro 协议的地方。两个二进制程序都通过一个小而精心挑选的公共接口调用它（见 `crates/lanius-core/src/lib.rs`）；`lanius-core` 内部的其他内容都是 `pub(crate)`，只能通过各模块自己的门面（facade）访问（例如 `crate::upstream::KiroHttpClient`、`crate::model::ModelResolver`）。

## 为什么需要网关

Kiro（Amazon Q Developer / AWS CodeWhisperer）使用自己的请求/响应协议，并基于自定义的 AWS event-stream 分帧传输，而不是 OpenAI Chat Completions API 或 Anthropic Messages API。Lanius 位于 Kiro 前方，对外同时暴露这两种常见的线上格式，因此现有的、使用 OpenAI 或 Anthropic 协议的工具、SDK 和编辑器集成，无需修改任何代码即可指向 Lanius 的 `/v1/...` 端点；而认证、协议转换以及 Kiro 实际后端的各种怪癖则由 Lanius 负责处理。

## 模块图（`lanius-core`）

```mermaid
graph LR
    lib["lib.rs<br/>crate 根；架构概览 + 精选的重导出"]

    subgraph api["api/ — 面向两种客户端协议的 HTTP 路由"]
        subgraph api_openai["openai/ — /v1/models, /v1/chat/completions"]
            api_openai_models["models.rs<br/>serde 线上类型"]
            api_openai_routes["routes.rs<br/>axum handler，OpenAiState"]
            api_openai_sse["sse.rs<br/>流式编码器 + 非流式响应构建器"]
        end
        subgraph api_anthropic["anthropic/ — /v1/messages, /v1/messages/count_tokens"]
            api_anthropic_models["models.rs<br/>serde 线上类型"]
            api_anthropic_routes["routes.rs<br/>axum handler，AnthropicState"]
            api_anthropic_sse["sse.rs<br/>流式状态机 + 非流式响应构建器"]
        end
    end

    subgraph convert["convert/ — 客户端线上格式 &lt;-&gt; Kiro payload 转换"]
        convert_core["core.rs<br/>与提供方无关的 UnifiedMessage/UnifiedTool + build_kiro_payload"]
        convert_openai["openai.rs<br/>OpenAI 请求 -&gt; UnifiedMessage/UnifiedTool"]
        convert_anthropic["anthropic.rs<br/>Anthropic 请求 -&gt; UnifiedMessage/UnifiedTool"]
        convert_guards["guards.rs<br/>payload 大小限制，裁剪后的 tool result 修复"]
    end

    subgraph auth["auth/ — 凭据加载、token 刷新、持久化"]
        auth_credentials["credentials.rs<br/>从环境变量 / JSON 文件 / kiro-cli SQLite 加载"]
        auth_refresh["refresh.rs<br/>实际的 refresh token HTTP 交换"]
    end

    subgraph upstream["upstream/ — 通过 HTTP 与 Kiro 通信并解码其响应"]
        upstream_client["client.rs<br/>带重试/退避的 HTTP 客户端（KiroHttpClient）"]
        upstream_parser["parser.rs<br/>AWS event-stream 片段解码器（AwsEventStreamParser）"]
        upstream_stream["stream.rs<br/>与提供方无关的 KiroEvent 流 + StreamResult 收集器"]
    end

    subgraph model["model/ — 模型目录与名称解析"]
        model_cache["cache.rs<br/>ModelInfoCache（带 TTL 的目录快照）"]
        model_resolver["resolver.rs<br/>ModelResolver（名称规范化/别名）"]
        model_reasoning["reasoning.rs<br/>各模型的原生 thinking/reasoning 能力"]
    end

    compat["compat.rs<br/>客户端兼容性钩子（工具名别名、host 改写、模型 id 格式化）"]
    truncation["truncation.rs<br/>检测并恢复上游输出截断"]
    tokenizer["tokenizer.rs<br/>Kiro 未报告用量时的 token 估算"]
    config["config.rs<br/>Config 结构体、环境变量解析、默认值、URL 模板"]
    error["error.rs<br/>GatewayError，Kiro/网络错误分类"]
    server["server.rs<br/>axum 应用组装、AppState、serve()/spawn()"]
    utils["utils.rs<br/>指纹、user-agent、id 生成、带空格的 JSON"]

    lib --> api
    lib --> convert
    lib --> auth
    lib --> upstream
    lib --> model
    lib --> compat
    lib --> truncation
    lib --> tokenizer
    lib --> config
    lib --> error
    lib --> server
    lib --> utils
```

## 数据流

```mermaid
flowchart TD
    client["客户端<br/>（任何兼容 OpenAI/Anthropic 的 SDK/工具）"]
    server_router["server.rs — axum Router<br/>CORS（仅 localhost）· panic 恢复 ·<br/>tracing · 为 Claude 客户端改写模型 id"]
    openai_routes["api::openai::routes<br/>（校验 bearer PROXY_API_KEY）"]
    anthropic_routes["api::anthropic::routes<br/>（校验 x-api-key/anthropic-version）"]
    convert_adapters["convert::{openai,anthropic}<br/>客户端消息/工具结构 -&gt; UnifiedMessage/UnifiedTool"]
    build_payload["convert::core::build_kiro_payload<br/>角色规范化 · 工具预处理 · 原生 reasoning 字段 ·<br/>convert::guards payload 大小裁剪/修复"]
    resolver["model::resolver::ModelResolver::resolve<br/>客户端模型名 -&gt; 具体的 Kiro 模型 id"]
    auth_manager["auth::AuthManager<br/>确保 bearer token 有效（必要时刷新）"]
    kiro_client["upstream::client::KiroHttpClient<br/>POST generateAssistantResponse，重试/退避，403 -&gt; 刷新"]
    aws_parser["upstream::parser::AwsEventStreamParser<br/>增量 JSON 片段解码 -&gt; ParserEvent<br/>（AWS event-stream 分帧字节）"]
    kiro_stream["upstream::stream::parse_kiro_stream<br/>ParserEvent -&gt; 与提供方无关的 KiroEvent（Content/Thinking/<br/>ToolUse/Usage/ContextUsage/Error）"]
    openai_sse["api::openai::sse<br/>（chunk/响应）"]
    anthropic_sse["api::anthropic::sse<br/>（SSE 状态机/响应）"]
    truncation_store["truncation::TruncationStore<br/>（记录并恢复因 Kiro 大小限制被截断的输出）"]

    client -->|HTTP| server_router
    server_router --> openai_routes
    server_router --> anthropic_routes
    openai_routes --> convert_adapters
    anthropic_routes --> convert_adapters
    convert_adapters --> build_payload
    build_payload --> resolver
    resolver --> auth_manager
    auth_manager --> kiro_client
    kiro_client --> aws_parser
    aws_parser --> kiro_stream
    kiro_stream --> openai_sse
    kiro_stream --> anthropic_sse
    openai_sse --> truncation_store
    anthropic_sse --> truncation_store
    truncation_store --> client
```

## 横切关注点

- **`config`**：每个可调参数都是一个带有文档化默认值的环境变量（见 [configuration.md](configuration.md)）；`Config::validate` 在启动时运行一次，若配置不安全或无效则拒绝提供服务。
- **`error`**：单个 `GatewayError` 枚举涵盖配置、认证、网络以及经上游分类的错误；`error::enhance_kiro_error` 将 Kiro 的原始错误 payload 转换为可操作的提示信息（例如将“超出上下文长度”与普通的 400 区分开）。
- **`compat`**：既不属于“结构转换”（`convert`）也不属于“凭据”（`auth`），但几乎适用于每个请求的行为：针对 Kiro 64 字符名称限制的工具名别名、针对仅限 control plane 操作的 host 改写，以及 Claude 客户端专用的模型 id 格式化。见 [compatibility.md](compatibility.md)。
- **`utils`**：与 Kiro 期望的线上格式逐字节匹配的逻辑（带空格的 JSON 分隔符、特定的 header 大小写/顺序、稳定的单机指纹）放在这里，因为 `convert` 和 `upstream` 都需要用到。

## 下一步阅读

- 端到端追踪一次请求：[request-flow.md](request-flow.md)
- 消息/工具结构如何转换：[conversion.md](conversion.md)
- 凭据与 token 刷新：[auth.md](auth.md)
- 解码 Kiro 的流式响应：[streaming.md](streaming.md)
