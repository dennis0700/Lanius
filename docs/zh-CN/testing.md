> [English](../testing.md) | 简体中文

# 测试

```sh
cargo test --workspace
```

会运行全部测试：与模块放在一起的单元测试、公共 API 示例上的文档测试（doctest）、各 crate `tests/` 目录下的集成测试，以及属性测试。不同类别的测试没有单独的命令，`cargo test --workspace` 会全部运行。

## 测试的位置

- **单元测试**：`lanius-core` 和 `lanius-gui` 中几乎每个模块都在文件末尾有一个 `#[cfg(test)] mod tests`，直接测试该模块的函数和类型（包括仅 `pub(crate)` 可见、集成测试无法访问的内部实现）。这部分构成了测试套件的主体。
- **文档测试**：大多数公共函数和类型都带有 `# Examples` 小节，其中包含可运行的代码块；`cargo test` 也会编译并运行它们。它们既是使用文档，也是针对所示 API 签名的回归测试。
- **集成测试**（`crates/*/tests/*.rs`）：以外部使用者的视角，从 crate 外部测试其公共 API：
  - `lanius-core/tests/tool_followup_payload.rs`：在完整的请求转换过程中，验证工具定义/工具结果别名的往返一致性（参见 [conversion.md](conversion.md) 和 [compatibility.md](compatibility.md#工具名别名)）。
  - `lanius-core/tests/parser_properties.rs`：针对 `AwsEventStreamParser` 的基于属性的测试（见下文）。
  - `lanius-cli/tests/gui_api_contract.rs`：契约测试，保护 `lanius-core` 的 `Config`/`server` 与 `lanius-gui` 之间的接口。`lanius-gui` 以库的形式嵌入网关，而不是把 `lanius-cli` 作为子进程启动。该测试专门用于在无需构建完整 Slint UI 的情况下，捕获对“GUI API 接口面”的破坏性变更（即桌面应用将其设置映射到哪些 `Config` 字段，以及它所驱动的 `spawn`/`shutdown` 生命周期）。其中包含一个端到端检查：在操作系统分配的临时端口上启动网关，请求 `/health`，优雅地关闭网关，并确认监听器确实已经关闭。

## 基于属性的测试（`proptest`）

`parser_properties.rs` 使用 `proptest` 验证 `AwsEventStreamParser` 与字节边界无关。这是一项核心正确性要求：Kiro 的原始字节流可能被网络栈任意切分，无论切分点落在哪里，解析器都必须产生相同的结果。它检查以下内容：

- `parse_is_independent_of_chunk_boundaries`：一次性输入同一 payload 与以任意分块大小输入，得到的事件完全相同。
- `byte_at_a_time_matches_whole`：逐字节输入与一次性输入整个 payload 的结果相同（这是最具对抗性的分块方式）。
- `multibyte_content_survives_arbitrary_splits`：包含多字节 UTF-8（CJK 字符）的 payload 在任意切分下都不会损坏，专门覆盖 `decode_into_buffer` 中“不要把一个 UTF-8 序列拆分到不同分块”的逻辑。
- `tool_arguments_are_split_independent`：以多个 `input` 片段流式传输的 tool call JSON 参数，无论这些片段在字节层面如何被再次分块，重组后的结果都相同。

同样的字节边界无关性，也可以针对真实捕获的响应手动验证：使用 `lanius probe --capture` + `lanius replay`，并尝试多个 `REPLAY_CHUNK_SIZE` 值。参见 [streaming.md](streaming.md#离线调试lanius-probe--lanius-replay)。

## 运行部分测试

```sh
# One crate
cargo test -p lanius-core

# One test (by substring match on the test name)
cargo test -p lanius-core is_content_truncated

# Doctests only
cargo test --workspace --doc

# Skip doctests (faster iteration on unit/integration tests)
cargo test --workspace --lib --tests
```

## 代码检查

```sh
cargo clippy --workspace --all-targets --all-features
```

`lanius-core` 的 `lib.rs` 在整个 crate 范围内设置了 `#![warn(clippy::all)]` 和 `#![forbid(unsafe_code)]`；`lanius-core` 中没有任何 `unsafe` 代码。`RUSTFLAGS="-W missing_docs" cargo build -p lanius-core --lib` 适合用来一次性检查公共项的文档注释覆盖情况（目前 `lanius-core` 中每个公共结构体字段、枚举变体和常量都有文档注释）。

## 编写新测试

遵循现有约定：凡是可以通过模块自身（可能是 `pub(crate)`）API 进行测试的内容，就在你修改的文件末尾放置一个 `#[cfg(test)] mod tests`；只有当被测对象确实跨模块，或专门涉及 crate 的*外部*契约时（例如 GUI 契约测试和 tool-followup 集成测试），才使用 `crates/*/tests/`。如果被测内容同时可以很好地充当函数调用方式的文档，优先使用文档测试而不是单元测试。
