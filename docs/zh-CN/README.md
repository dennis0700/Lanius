> [English](../README.md) | 简体中文

# Lanius 技术文档

本目录包含 Lanius 的实现层文档，作为 crate 级 rustdoc 注释（`cargo doc --open -p
lanius-core`）的补充，从更高的层面说明各部分如何协同工作。

建议先阅读 [architecture.md](architecture.md) 了解整体系统设计，再根据手头的工作
深入对应主题：

| 文档 | 内容 |
|---|---|
| [architecture.md](architecture.md) | 工作区结构、请求生命周期、模块地图、数据流图 |
| [request-flow.md](request-flow.md) | 逐步追踪一个请求从 HTTP 入口到返回客户端响应的全过程 |
| [conversion.md](conversion.md) | OpenAI/Anthropic ↔ Kiro payload 转换管线（`convert`） |
| [auth.md](auth.md) | 凭据来源、token 刷新流程、profile ARN 自动获取、持久化 |
| [streaming.md](streaming.md) | Kiro 的 AWS event-stream 封帧、增量解析器以及 SSE 重新编码 |
| [compatibility.md](compatibility.md) | 客户端兼容性钩子：工具名别名、主机改写、模型 ID 格式化 |
| [model-catalog.md](model-catalog.md) | 模型名称解析/规范化，以及原生 reasoning/thinking 支持 |
| [truncation.md](truncation.md) | 检测并恢复上游 tool call/内容截断 |
| [configuration.md](configuration.md) | 所有环境变量、默认值及其作用 |
| [deployment.md](deployment.md) | 在 Linux 上安装 `lanius-cli` 并以 systemd 服务运行 |
| [gui.md](gui.md) | `lanius-gui` 桌面应用架构（Slint UI、控制器、内嵌网关） |
| [testing.md](testing.md) | 测试套件的组织方式，以及如何运行和扩展测试 |

每篇文档默认读者已熟悉阅读顺序中位于其前面的文档，但如果你已了解相关领域，也可以
单独阅读。
