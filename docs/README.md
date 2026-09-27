English | [简体中文](zh-CN/README.md)

# Lanius Technical Documentation

This directory contains implementation-level documentation for Lanius,
supplementing the crate-level rustdoc comments (`cargo doc --open -p
lanius-core`) with a higher-level view of how the pieces fit together.

Start with [architecture.md](architecture.md) for the overall system design,
then drill into the topic that matches what you're working on:

| Document | Covers |
|---|---|
| [architecture.md](architecture.md) | Workspace layout, request lifecycle, module map, data-flow diagram |
| [request-flow.md](request-flow.md) | Step-by-step trace of a request from HTTP ingress to the client response |
| [conversion.md](conversion.md) | The OpenAI/Anthropic ↔ Kiro payload conversion pipeline (`convert`) |
| [auth.md](auth.md) | Credential sources, token refresh flows, profile ARN autofetch, persistence |
| [streaming.md](streaming.md) | Kiro's AWS event-stream framing, the incremental parser, and SSE re-encoding |
| [compatibility.md](compatibility.md) | Client-compatibility hooks: tool-name aliasing, host rewriting, model-id formatting |
| [model-catalog.md](model-catalog.md) | Model name resolution/normalization and native reasoning/thinking support |
| [truncation.md](truncation.md) | Detecting and recovering from upstream tool-call/content truncation |
| [configuration.md](configuration.md) | Every environment variable, its default, and what it controls |
| [deployment.md](deployment.md) | Installing `lanius-cli` on Linux and running it as a systemd service |
| [gui.md](gui.md) | `lanius-gui` desktop app architecture (Slint UI, controller, embedded gateway) |
| [testing.md](testing.md) | How the test suite is organized and how to run/extend it |

Each document assumes familiarity with the previous ones in reading order,
but can also be read standalone if you already know the area.
