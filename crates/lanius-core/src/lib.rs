//! `lanius-core` is the engine behind Lanius: an OpenAI- and Anthropic-compatible
//! API gateway that sits in front of the Kiro backend (Amazon Q Developer /
//! AWS CodeWhisperer) and lets existing OpenAI/Anthropic clients talk to it
//! transparently.
//!
//! # Architecture and data flow
//!
//! A request flows through the crate roughly like this:
//!
//! 1. **Ingress (`api`, `server`)** — [`api::openai`] and [`api::anthropic`]
//!    expose HTTP routes that accept requests in each provider's wire format.
//!    [`server`] wires those routers together behind a single `axum` app,
//!    adding CORS, panic recovery, tracing and proxy-key authentication.
//! 2. **Conversion (`convert`)** — incoming OpenAI/Anthropic payloads are
//!    translated into Kiro's internal request shape (and back again for
//!    responses) by the converters in [`convert`], with [`convert::guards`]
//!    enforcing invariants along the way.
//! 3. **Model resolution + auth (`model`, `auth`)** — the requested model
//!    name is normalized and resolved to a concrete Kiro model id via
//!    [`model::resolver`], using catalog data cached by [`model::cache`].
//!    [`auth`] keeps the single configured account's OAuth/SSO tokens fresh.
//! 4. **Upstream call (`upstream`)** — [`upstream::client`] sends the
//!    converted request to Kiro over HTTP with retry/backoff, and
//!    [`upstream::parser`] / [`upstream::stream`] decode Kiro's AWS
//!    event-stream response (including bracket-style `[Called ...]` tool
//!    calls) into a provider-agnostic sequence of events.
//! 5. **Post-processing** — [`truncation`] detects and helps recover from
//!    upstream truncation, and [`tokenizer`] estimates token counts when
//!    Kiro does not report them. Native thinking/reasoning is requested per
//!    model according to its catalog schema ([`model::ReasoningCapability`])
//!    and arrives as its own upstream event.
//! 6. **Egress** — the converters in [`convert`] (driven from [`api`]) turn
//!    the resulting events back into OpenAI- or Anthropic-shaped
//!    responses/SSE streams for the client.
//!
//! Cross-cutting concerns live in [`config`] (environment-driven
//! configuration and defaults), [`error`] (the unified [`GatewayError`] type
//! plus Kiro/network error classification), and [`utils`] (fingerprinting,
//! user-agent strings, id generation, and spaced-JSON formatting used to
//! match Kiro's expected wire format byte-for-byte).

#![forbid(unsafe_code)]
#![warn(clippy::all)]

/// HTTP-facing API surface: OpenAI- and Anthropic-compatible route handlers.
pub mod api;
/// OAuth/SSO authentication and token refresh against Kiro/AWS.
pub mod auth;
/// Client-compatibility shims (e.g. rewriting model ids for specific clients).
pub mod compat;
/// Environment-driven configuration structs, defaults, and constants.
pub mod config;
/// Bidirectional conversion between OpenAI/Anthropic and Kiro request/response shapes.
pub mod convert;
/// Unified error type and Kiro/network error classification and enhancement.
pub mod error;
/// Model catalog caching and model-name resolution/normalization.
pub mod model;
/// `axum`-based HTTP server: route aggregation, CORS, and auth middleware.
pub mod server;
/// Token estimation for requests when Kiro does not report usage.
pub mod tokenizer;
/// Cache and recovery-prompt helpers for content/tool-call truncation.
pub mod truncation;
/// Kiro upstream HTTP client, AWS event-stream parsing, and unified stream events.
pub mod upstream;
/// Fingerprinting, user-agent, id generation, and spaced-JSON helpers.
pub mod utils;

pub use config::Config;
pub use error::{GatewayError, Result};

// A small, curated set of external-facing re-exports for integration tests and
// `lanius-cli`: the OpenAI wire-format request type and the Kiro payload builder
// that operates on it, plus the tool-name-alias table needed to call it. Everything
// else under `api`/`convert`/`model`/`upstream` is `pub(crate)` and reached only
// through each module's own facade (see e.g. `crate::upstream::KiroHttpClient`).
pub use api::openai::ChatCompletionRequest;
pub use compat::ToolNameAliases;
pub use convert::build_kiro_payload;
