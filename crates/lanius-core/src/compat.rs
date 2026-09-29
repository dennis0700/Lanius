//! Request/response compatibility hook pipeline sitting between Lanius's OpenAI/Anthropic
//! API surface and the Kiro backend.
//!
//! Kiro (and the AWS CodeWhisperer control plane behind it) has a handful of quirks that
//! don't belong in [`crate::convert`] (which is about translating message *shapes*) or in
//! [`crate::auth`] (which is about *credentials*), but still need to happen on essentially
//! every request/response. This module collects those quirks as small, independently
//! testable [`RequestHook`]/[`ResponseHook`] implementations, run in order by a
//! [`CompatibilityPipeline`]:
//!
//! - **Tool name aliasing** ([`ToolNameAliases`]) — Kiro restricts tool names to 64
//!   ASCII-alphanumeric/`_`/`-` characters, but OpenAI/Anthropic clients routinely send
//!   longer or differently-formatted names (e.g. MCP-style `plugin__server__tool`). This
//!   type deterministically maps such names to a short, collision-avoiding alias before
//!   they reach Kiro, and reverses the mapping in tool-call responses and in
//!   `[Called <name> with args:...]`-style text markers, including a streaming-safe
//!   variant ([`ToolNameAliases::restore_text_fragment`]) that withholds enough trailing
//!   text to avoid splitting a marker across chunks.
//! - **Profile ARN autofetch** ([`ProfileArnAutofetchHook`]) — a shared, at-most-once
//!   "claim" primitive and the raw HTTP request-shape helpers for discovering a Kiro
//!   profile ARN. The actual HTTP call is issued by [`crate::auth::AuthManager`], which
//!   reuses this type so the "only try once" logic isn't duplicated.
//! - **Host rewriting** ([`control_plane_host`], [`chat_host`], `ControlPlaneHostHook`,
//!   `ChatHostFallbackHook`) — some control-plane-only operations (listing models,
//!   listing usage limits, MCP) must always go to the AWS `q.<region>.amazonaws.com` host
//!   even when the account would otherwise use the paid `runtime.<region>.kiro.dev` host,
//!   and profile-less accounts must always fall back to the control-plane host for chat
//!   too, since the paid runtime host requires a profile ARN.
//! - **Model ID formatting** (`ModelIdFormatHook`, [`rewrite_model_ids`],
//!   [`is_claude_client`]) — Claude Code and similar clients expect model IDs with dashes
//!   (`claude-sonnet-4-6`) rather than the dotted form Kiro's `/v1/models` endpoint returns
//!   (`claude-sonnet-4.6`); this hook rewrites `GET /v1/models` responses only when the
//!   request appears to come from a Claude-branded client.
//!
//! [`CompatibilityPipeline::new`] wires up the default hook set in the order they should
//! run, and [`crate::server`] / the `api::*` handlers call
//! [`CompatibilityPipeline::apply_request`] / [`CompatibilityPipeline::apply_response`]
//! around the proxied call to Kiro. Note that several hooks here (`ToolNameAliasHook`,
//! `ChatHostFallbackHook`) are currently no-op placeholders in the pipeline itself because
//! their actual logic is invoked directly by [`crate::convert`] and [`crate::auth`]
//! respectively — they exist in the pipeline mainly so [`CompatibilityPipeline::request_hook_names`]
//! accurately reports which compatibility behaviors are active.

use crate::error::Result;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// Maximum tool name length Kiro's `toolSpecification.name` field accepts.
pub const MAX_KIRO_TOOL_NAME_LENGTH: usize = 64;

/// An outbound request as it is about to be sent upstream to Kiro, mutable so
/// [`RequestHook`]s can adjust its destination, headers, or body in place.
#[derive(Clone, Debug)]
pub struct CompatRequest {
    /// The HTTP method of the outbound request.
    pub method: Method,
    /// The upstream path being requested.
    pub path: String,
    /// The request's headers, mutable in place by hooks.
    pub headers: HeaderMap,
    /// The JSON request body, mutable in place by hooks.
    pub body: serde_json::Value,
    /// Override for the upstream URL, if a hook needs to redirect the
    /// request (e.g. to a different control-plane host).
    pub upstream_url: Option<String>,
}

/// An inbound response received from Kiro, mutable so [`ResponseHook`]s can rewrite its
/// body/headers before it is relayed back to the client.
#[derive(Clone, Debug)]
pub struct CompatResponse {
    /// The HTTP status Kiro returned.
    pub status: StatusCode,
    /// The response's headers, mutable in place by hooks.
    pub headers: HeaderMap,
    /// The raw response body bytes, mutable in place by hooks.
    pub body: Vec<u8>,
}

/// A single, independently named and toggleable request-side compatibility adjustment.
pub trait RequestHook: Send + Sync {
    /// Stable identifier used for diagnostics (see
    /// [`CompatibilityPipeline::request_hook_names`]), not shown to end users.
    fn name(&self) -> &'static str;
    /// Whether this hook should run at all; defaults to always-enabled.
    fn enabled(&self) -> bool {
        true
    }
    /// Applies this hook's adjustment to `request` in place.
    fn apply(&self, request: &mut CompatRequest) -> Result<()>;
}

/// A single, independently named and toggleable response-side compatibility adjustment.
pub trait ResponseHook: Send + Sync {
    /// Stable identifier used for diagnostics (see
    /// [`CompatibilityPipeline::response_hook_names`]), not shown to end users.
    fn name(&self) -> &'static str;
    /// Whether this hook should run at all; defaults to always-enabled.
    fn enabled(&self) -> bool {
        true
    }
    /// Applies this hook's adjustment to `response` in place. `request` is provided
    /// read-only context (e.g. to check the original path/headers).
    fn apply(&self, request: &CompatRequest, response: &mut CompatResponse) -> Result<()>;
}

/// An ordered collection of [`RequestHook`]s and [`ResponseHook`]s applied around every
/// proxied call to Kiro.
#[derive(Default)]
pub struct CompatibilityPipeline {
    request_hooks: Vec<Box<dyn RequestHook>>,
    response_hooks: Vec<Box<dyn ResponseHook>>,
}

impl CompatibilityPipeline {
    /// Builds the default pipeline with the standard set of hooks, in the order they run
    /// for requests (response hooks run in reverse order — see
    /// [`CompatibilityPipeline::apply_response`] — so that the last-registered response
    /// hook actually runs first, mirroring the request pipeline's outermost-to-innermost
    /// wrapping intuition).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::CompatibilityPipeline;
    ///
    /// let pipeline = CompatibilityPipeline::new();
    /// assert!(!pipeline.request_hook_names().is_empty());
    /// ```
    pub fn new() -> Self {
        let mut pipeline = Self::default();
        pipeline.request_hooks.push(Box::new(ToolNameAliasHook));
        pipeline
            .request_hooks
            .push(Box::new(ProfileArnAutofetchHook::default()));
        pipeline.request_hooks.push(Box::new(ControlPlaneHostHook));
        pipeline.request_hooks.push(Box::new(ChatHostFallbackHook));
        pipeline.response_hooks.push(Box::new(ModelIdFormatHook));
        pipeline
    }

    /// Names of all registered request hooks, in application order.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::CompatibilityPipeline;
    ///
    /// let pipeline = CompatibilityPipeline::new();
    /// assert!(pipeline.request_hook_names().contains(&"tool_name_alias"));
    /// ```
    pub fn request_hook_names(&self) -> Vec<&'static str> {
        self.request_hooks.iter().map(|hook| hook.name()).collect()
    }

    /// Names of all registered response hooks, in registration order (note: this is *not*
    /// the order they execute in — see [`CompatibilityPipeline::apply_response`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::CompatibilityPipeline;
    ///
    /// let pipeline = CompatibilityPipeline::new();
    /// assert!(pipeline.response_hook_names().contains(&"model_id_format"));
    /// ```
    pub fn response_hook_names(&self) -> Vec<&'static str> {
        self.response_hooks.iter().map(|hook| hook.name()).collect()
    }

    /// Runs every enabled request hook, in registration order, against `request`. Stops
    /// and propagates the first error encountered.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::{CompatRequest, CompatibilityPipeline};
    /// use axum::http::{HeaderMap, Method};
    /// use serde_json::json;
    ///
    /// let pipeline = CompatibilityPipeline::new();
    /// let mut request = CompatRequest {
    ///     method: Method::POST,
    ///     path: "/v1/chat/completions".into(),
    ///     headers: HeaderMap::new(),
    ///     body: json!({}),
    ///     upstream_url: None,
    /// };
    /// assert!(pipeline.apply_request(&mut request).is_ok());
    /// ```
    pub fn apply_request(&self, request: &mut CompatRequest) -> Result<()> {
        for hook in &self.request_hooks {
            if hook.enabled() {
                hook.apply(request)?;
            }
        }
        Ok(())
    }

    /// Runs every enabled response hook against `response`, in *reverse* registration
    /// order. Stops and propagates the first error encountered.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::{CompatRequest, CompatResponse, CompatibilityPipeline};
    /// use axum::http::{HeaderMap, Method, StatusCode};
    /// use serde_json::json;
    ///
    /// let pipeline = CompatibilityPipeline::new();
    /// let request = CompatRequest {
    ///     method: Method::GET,
    ///     path: "/v1/models".into(),
    ///     headers: HeaderMap::new(),
    ///     body: json!(null),
    ///     upstream_url: None,
    /// };
    /// let mut response = CompatResponse {
    ///     status: StatusCode::OK,
    ///     headers: HeaderMap::new(),
    ///     body: br#"{"data":[]}"#.to_vec(),
    /// };
    /// assert!(pipeline.apply_response(&request, &mut response).is_ok());
    /// ```
    pub fn apply_response(
        &self,
        request: &CompatRequest,
        response: &mut CompatResponse,
    ) -> Result<()> {
        for hook in self.response_hooks.iter().rev() {
            if hook.enabled() {
                hook.apply(request, response)?;
            }
        }
        Ok(())
    }
}

/// Maintains a bidirectional, deterministic mapping between original tool names (which
/// may be too long or contain characters Kiro's `toolSpecification.name` rejects) and
/// short, Kiro-legal aliases.
///
/// A single instance is meant to live for the duration of one request/response cycle
/// (constructed fresh per request) so that the same original name always maps to the same
/// alias within that cycle, and vice versa for restoring aliases back to original names in
/// the response.
#[derive(Clone, Debug, Default)]
pub struct ToolNameAliases {
    alias_to_original: HashMap<String, String>,
    original_to_alias: HashMap<String, String>,
    reserved: HashSet<String>,
}

impl ToolNameAliases {
    /// Whether `name` violates Kiro's tool-name constraints (empty, longer than
    /// [`MAX_KIRO_TOOL_NAME_LENGTH`], or containing any byte outside
    /// ASCII-alphanumeric/`_`/`-`) and therefore needs aliasing.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::ToolNameAliases;
    ///
    /// assert!(!ToolNameAliases::needs_alias("read_file"));
    /// assert!(ToolNameAliases::needs_alias("has spaces"));
    /// ```
    pub fn needs_alias(name: &str) -> bool {
        name.is_empty()
            || name.len() > MAX_KIRO_TOOL_NAME_LENGTH
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }

    /// Pre-registers a batch of tool names before any individual aliasing happens.
    ///
    /// This two-pass approach (reserve legal names first, then alias in a second pass)
    /// matters when a tool set contains both an already-legal name and an illegal name
    /// whose generated alias would happen to collide with that legal name: reserving the
    /// legal names up front (first loop) ensures [`ToolNameAliases::alias_for`] never picks
    /// an alias that shadows a name the client is actually using verbatim.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::ToolNameAliases;
    ///
    /// let mut aliases = ToolNameAliases::default();
    /// aliases.register_names(["read_file", "has spaces"]);
    /// // Already-legal names are reserved unchanged; illegal ones get an alias assigned.
    /// assert_eq!(aliases.alias_for("read_file"), "read_file");
    /// ```
    pub fn register_names<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        let names: Vec<&str> = names.into_iter().collect();
        for name in &names {
            if !Self::needs_alias(name) {
                self.reserved.insert((*name).to_string());
            }
        }
        for name in names {
            let _ = self.alias_for(name);
        }
    }

    /// Returns the Kiro-legal name to use for `name`: `name` itself if it is already
    /// legal, the previously assigned alias if one exists, or else a freshly generated one.
    ///
    /// Alias generation: a `t_<sha256-prefix>_<sanitized-suffix>` name is built (see
    /// `build_alias`) starting with a 12-hex-character digest prefix; if that alias is
    /// already reserved by a different original name or already claimed as an alias for a
    /// *different* original name, the digest prefix is lengthened by 4 hex characters and
    /// retried. Because the digest is a cryptographic hash of the full original name, this
    /// loop terminates in practice extremely quickly (a collision at the same prefix length
    /// for two different inputs is astronomically unlikely) while remaining directionally
    /// correct up to the full digest length.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::ToolNameAliases;
    ///
    /// let mut aliases = ToolNameAliases::default();
    /// let alias = aliases.alias_for("has spaces");
    /// assert!(alias.len() <= 64);
    /// assert_eq!(aliases.original_for(&alias), "has spaces");
    /// ```
    pub fn alias_for(&mut self, name: &str) -> String {
        if !Self::needs_alias(name) {
            return name.to_string();
        }
        if let Some(alias) = self.original_to_alias.get(name) {
            return alias.clone();
        }
        let mut digest_len = 12;
        loop {
            let alias = build_alias(name, digest_len);
            if !self.reserved.contains(&alias)
                && self
                    .alias_to_original
                    .get(&alias)
                    .is_none_or(|original| original == name)
            {
                self.alias_to_original
                    .insert(alias.clone(), name.to_string());
                self.original_to_alias
                    .insert(name.to_string(), alias.clone());
                return alias;
            }
            digest_len += 4;
        }
    }

    /// Reverses [`ToolNameAliases::alias_for`]: returns the original name for a known
    /// alias, or `name` unchanged if it is not a known alias (e.g. it was already legal and
    /// never needed one).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::ToolNameAliases;
    ///
    /// let aliases = ToolNameAliases::default();
    /// // An unknown name is returned unchanged.
    /// assert_eq!(aliases.original_for("read_file"), "read_file");
    /// ```
    pub fn original_for(&self, name: &str) -> String {
        self.alias_to_original
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// Restores any aliased tool names appearing inside free-form `text` (both structured
    /// tool-call fields and `[Called <alias> with args:...]`-style inline markers, via
    /// [`ToolNameAliases::restore_response_value`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::ToolNameAliases;
    ///
    /// let mut aliases = ToolNameAliases::default();
    /// let alias = aliases.alias_for("has spaces");
    /// let text = format!("[Called {alias} with args: {{}}]");
    /// assert!(aliases.restore_text(&text).contains("has spaces"));
    /// ```
    pub fn restore_text(&self, text: &str) -> String {
        let mut value = serde_json::Value::String(text.to_owned());
        self.restore_response_value(&mut value);
        value.as_str().unwrap_or(text).to_owned()
    }

    /// Streaming-safe variant of [`ToolNameAliases::restore_text`]: appends `fragment` to
    /// `pending` and returns the portion of `pending` that is now safe to flush.
    ///
    /// The tricky part of streaming alias restoration is that a `[Called <alias> with
    /// args:` marker could be split across two separate stream chunks, and if we flush text
    /// eagerly we might emit half an alias before we've seen the rest of it (and so never
    /// get the chance to restore it to the original name). To avoid that, unless
    /// `final_fragment` is set, this method always withholds a suffix of `pending` at least
    /// as long as twice the longest possible marker string (`longest_marker`, computed from
    /// the known aliases) — comfortably enough slack that a marker beginning anywhere in the
    /// withheld tail cannot yet be known to be complete. Everything before that withheld
    /// tail is restored and returned; the tail itself is left in `pending` for the next
    /// call. When `final_fragment` is true (end of stream), the entire remaining buffer is
    /// flushed regardless.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::ToolNameAliases;
    ///
    /// let mut aliases = ToolNameAliases::default();
    /// let alias = aliases.alias_for("has spaces");
    /// let mut pending = String::new();
    /// let marker = format!("[Called {alias} with args: {{}}]");
    /// // Flushing the final fragment restores the alias to its original name.
    /// let flushed = aliases.restore_text_fragment(&mut pending, &marker, true);
    /// assert!(flushed.contains("has spaces"));
    /// ```
    pub fn restore_text_fragment(
        &self,
        pending: &mut String,
        fragment: &str,
        final_fragment: bool,
    ) -> String {
        pending.push_str(fragment);
        if final_fragment {
            return self.restore_text(&std::mem::take(pending));
        }
        let longest_marker = self
            .alias_to_original
            .keys()
            .map(|alias| format!("[Called {alias} with args:").chars().count())
            .max()
            .unwrap_or(0);
        let keep = longest_marker.saturating_mul(2);
        let char_count = pending.chars().count();
        if char_count <= keep {
            return String::new();
        }
        let split_at = char_count.saturating_sub(keep);
        let byte_index = pending
            .char_indices()
            .nth(split_at)
            .map(|(index, _)| index)
            .unwrap_or(pending.len());
        let tail = pending.split_off(byte_index);
        let ready = std::mem::replace(pending, tail);
        self.restore_text(&ready)
    }

    /// Applies aliasing to an entire outbound request JSON value: first collects every
    /// declared tool spec name and registers them all up front (so [`ToolNameAliases::register_names`]'s
    /// collision-avoidance sees the complete tool set before any name is actually aliased),
    /// then walks the whole value replacing tool names wherever they appear (tool
    /// declarations and inline tool-call/tool-use references).
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::ToolNameAliases;
    /// use serde_json::json;
    ///
    /// let mut aliases = ToolNameAliases::default();
    /// let mut request = json!({"tools": [{"name": "has spaces"}]});
    /// aliases.alias_request_value(&mut request);
    /// assert_ne!(request["tools"][0]["name"], "has spaces");
    /// ```
    pub fn alias_request_value(&mut self, value: &mut serde_json::Value) {
        let mut names = Vec::new();
        collect_tool_spec_names(value, &mut names);
        self.register_names(names.iter().map(String::as_str));
        alias_tool_fields(value, self);
    }

    /// Applies the reverse mapping to an entire inbound response JSON value: restores
    /// structured tool-name fields, then restores any `[Called <alias> with args:...]`
    /// text markers found anywhere in the value.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::ToolNameAliases;
    /// use serde_json::json;
    ///
    /// let mut aliases = ToolNameAliases::default();
    /// let alias = aliases.alias_for("has spaces");
    /// let mut response = json!({"tool_calls": [{"function": {"name": alias}}]});
    /// aliases.restore_response_value(&mut response);
    /// assert_eq!(response["tool_calls"][0]["function"]["name"], "has spaces");
    /// ```
    pub fn restore_response_value(&self, value: &mut serde_json::Value) {
        restore_tool_fields(value, self);
        restore_bracket_calls(value, self);
    }
}

/// Generates a candidate alias for `name` using a SHA-256 digest of the full name
/// (truncated to `digest_len` hex characters) plus a human-readable suffix derived from
/// the tail of `name`, so the alias stays recognizable while staying within
/// [`MAX_KIRO_TOOL_NAME_LENGTH`].
///
/// Suffix derivation: if `name` contains `__`, only the text after the *last* `__` is used
/// as the suffix source (this matches MCP-style `plugin__server__tool_name` naming, where
/// the final segment is the most meaningful part); otherwise the whole name is used. Any
/// character outside ASCII-alphanumeric/`_`/`-` is collapsed to a single `_` (consecutive
/// invalid characters do not produce multiple underscores), leading/trailing `_`/`-` are
/// trimmed, and an empty result falls back to the literal suffix `"tool"`.
///
/// Length budget: the fixed prefix `t_<digest>_` is computed first, and the suffix is then
/// truncated (keeping its *end*, not its start, since the end is usually the more specific
/// part of a namespaced name) to whatever byte budget remains under
/// [`MAX_KIRO_TOOL_NAME_LENGTH`].
fn build_alias(name: &str, digest_len: usize) -> String {
    let digest = hex::encode(Sha256::digest(name.as_bytes()));
    let suffix_source =
        name.rsplit_once("__").map_or(
            name,
            |(_, suffix)| {
                if suffix.is_empty() { name } else { suffix }
            },
        );
    let mut suffix = String::new();
    let mut previous_replaced = false;
    for character in suffix_source.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            suffix.push(character);
            previous_replaced = false;
        } else if !previous_replaced {
            suffix.push('_');
            previous_replaced = true;
        }
    }
    let suffix = suffix.trim_matches(['_', '-']);
    let suffix = if suffix.is_empty() { "tool" } else { suffix };
    let prefix = format!("t_{}_", &digest[..digest_len.min(digest.len())]);
    let max_suffix_bytes = MAX_KIRO_TOOL_NAME_LENGTH.saturating_sub(prefix.len());
    // Keep the *tail* of the suffix (iterate in reverse, then reverse back) so truncation
    // preserves the most specific/identifying part of a long namespaced tool name.
    let tail: String = suffix
        .chars()
        .rev()
        .scan(0usize, |used, character| {
            let size = character.len_utf8();
            if *used + size > max_suffix_bytes {
                None
            } else {
                *used += size;
                Some(character)
            }
        })
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}{tail}")
}

/// Collects every tool name declared in a request's top-level `tools` array, supporting
/// both the OpenAI (`{"function":{"name":...}}`) and Anthropic (`{"name":...}`) shapes.
fn collect_tool_spec_names(value: &serde_json::Value, names: &mut Vec<String>) {
    let Some(tools) = value.get("tools").and_then(serde_json::Value::as_array) else {
        return;
    };
    for tool in tools {
        let name = tool
            .get("function")
            .and_then(|function| function.get("name"))
            .or_else(|| tool.get("name"))
            .and_then(serde_json::Value::as_str);
        if let Some(name) = name {
            names.push(name.to_string());
        }
    }
}

/// Aliases the top-level `tools` array entries, then recurses through the rest of the
/// value to alias any inline tool-call/tool-use references.
fn alias_tool_fields(value: &mut serde_json::Value, aliases: &mut ToolNameAliases) {
    if let Some(tools) = value
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        for tool in tools {
            replace_name(tool, aliases, true);
        }
    }
    alias_tool_fields_recursive(value, aliases);
}

/// Recursively walks `value`, treating any object that looks like a tool-use/tool-call
/// reference (has `toolUseId`, `tool_use_id`, `tool_calls`, or `function`) as a name to
/// alias, then continues into all children regardless so nested tool references at any
/// depth are covered.
fn alias_tool_fields_recursive(value: &mut serde_json::Value, aliases: &mut ToolNameAliases) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                alias_tool_fields_recursive(item, aliases);
            }
        }
        serde_json::Value::Object(object) => {
            let is_tool_use = object.contains_key("toolUseId")
                || object.contains_key("tool_use_id")
                || object.contains_key("tool_calls")
                || object.contains_key("function");
            if is_tool_use {
                replace_object_name(object, aliases, true);
            }
            for child in object.values_mut() {
                alias_tool_fields_recursive(child, aliases);
            }
        }
        _ => {}
    }
}

fn replace_name(value: &mut serde_json::Value, aliases: &mut ToolNameAliases, inbound: bool) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    replace_object_name(object, aliases, inbound);
}

/// Replaces the `name` field of `object` (and, if present, its nested `function.name`)
/// using either the alias-for (`inbound = true`, outbound request direction) or
/// original-for (`inbound = false`, inbound response direction) mapping.
fn replace_object_name(
    object: &mut serde_json::Map<String, serde_json::Value>,
    aliases: &mut ToolNameAliases,
    inbound: bool,
) {
    if let Some(function) = object.get_mut("function") {
        replace_name(function, aliases, inbound);
    }
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(name) = name {
        let replacement = if inbound {
            aliases.alias_for(&name)
        } else {
            aliases.original_for(&name)
        };
        object.insert("name".to_string(), serde_json::Value::String(replacement));
    }
}

/// Recursively walks `value`, restoring any tool-name field (`function.name`, or a
/// top-level `name` on an object that looks like a tool-use/tool-call/tool-result) back to
/// its original (pre-alias) form.
fn restore_tool_fields(value: &mut serde_json::Value, aliases: &ToolNameAliases) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                restore_tool_fields(item, aliases);
            }
        }
        serde_json::Value::Object(object) => {
            let is_tool = object.contains_key("toolUseId")
                || object.contains_key("tool_use_id")
                || object.contains_key("tool_calls")
                || object.contains_key("function")
                || object.get("type").and_then(serde_json::Value::as_str) == Some("tool_use");
            if is_tool {
                if let Some(function) = object.get_mut("function") {
                    restore_function_name(function, aliases);
                }
                let name = object
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
                if let Some(name) = name {
                    object.insert(
                        "name".to_string(),
                        serde_json::Value::String(aliases.original_for(&name)),
                    );
                }
            }
            for child in object.values_mut() {
                restore_tool_fields(child, aliases);
            }
        }
        _ => {}
    }
}

fn restore_function_name(value: &mut serde_json::Value, aliases: &ToolNameAliases) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(name) = name {
        object.insert(
            "name".to_string(),
            serde_json::Value::String(aliases.original_for(&name)),
        );
    }
}

/// Recursively walks `value`, rewriting every occurrence of `[Called <alias> with args:`
/// inline text markers back to `[Called <original> with args:` for every known alias. This
/// covers cases (some model outputs) where the tool name appears embedded in free text
/// rather than in a structured tool-call field.
fn restore_bracket_calls(value: &mut serde_json::Value, aliases: &ToolNameAliases) {
    match value {
        serde_json::Value::String(text) => {
            for (alias, original) in &aliases.alias_to_original {
                let needle = format!("[Called {alias} with args:");
                let replacement = format!("[Called {original} with args:");
                if text.contains(&needle) {
                    *text = text.replace(&needle, &replacement);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                restore_bracket_calls(item, aliases);
            }
        }
        serde_json::Value::Object(object) => {
            for child in object.values_mut() {
                restore_bracket_calls(child, aliases);
            }
        }
        _ => {}
    }
}

/// Placeholder pipeline entry for tool-name aliasing. The real aliasing logic lives on
/// [`ToolNameAliases`] and is invoked directly by [`crate::convert`] during payload
/// construction (before this pipeline runs), so this hook is a no-op — it exists only so
/// `"tool_name_alias"` shows up in [`CompatibilityPipeline::request_hook_names`] as an
/// active compatibility behavior.
struct ToolNameAliasHook;
impl RequestHook for ToolNameAliasHook {
    fn name(&self) -> &'static str {
        "tool_name_alias"
    }
    fn apply(&self, request: &mut CompatRequest) -> Result<()> {
        let _ = request;
        Ok(())
    }
}

/// Shared "attempt at most once" primitive and wire-format helpers for auto-discovering a
/// Kiro profile ARN. The atomic flag makes [`ProfileArnAutofetchHook::claim_fetch`] safe to
/// call from multiple concurrent requests without duplicate lookups; the actual HTTP call
/// is made by `crate::auth::AuthManager::autofetch_profile_arn`, which holds an instance
/// of this hook for its lifetime.
#[derive(Default)]
pub struct ProfileArnAutofetchHook {
    attempted: std::sync::atomic::AtomicBool,
}
impl ProfileArnAutofetchHook {
    /// Atomically claims the right to perform the autofetch: returns `true` at most once
    /// (across all calls on this instance) and only when a token is present and no profile
    /// is already known. Every other call — including calls after a claim already
    /// succeeded — returns `false`, whether or not the eventual fetch attempt actually
    /// succeeded, since retrying is not automatically safe if the caller-provided
    /// conditions still hold.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::ProfileArnAutofetchHook;
    ///
    /// let hook = ProfileArnAutofetchHook::default();
    /// assert!(hook.claim_fetch(true, false));
    /// // A second call never claims again, even with the same arguments.
    /// assert!(!hook.claim_fetch(true, false));
    /// ```
    pub fn claim_fetch(&self, token_present: bool, profile_present: bool) -> bool {
        token_present
            && !profile_present
            && !self
                .attempted
                .swap(true, std::sync::atomic::Ordering::AcqRel)
    }
    /// The control-plane URL for the `ListAvailableProfiles` API in `region`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::ProfileArnAutofetchHook;
    ///
    /// assert_eq!(
    ///     ProfileArnAutofetchHook::profile_url("eu-west-1"),
    ///     "https://q.eu-west-1.amazonaws.com/"
    /// );
    /// ```
    pub fn profile_url(region: &str) -> String {
        format!("https://q.{region}.amazonaws.com/")
    }
    /// Builds the header set for a `ListAvailableProfiles` request: bearer auth using
    /// `token`, and the AWS SDK-style target/user-agent headers (including `fingerprint`,
    /// the per-machine identifier) that the control plane expects.
    ///
    /// Callers must treat `token` as sensitive: it is embedded verbatim in the
    /// `Authorization` header value returned here.
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::ProfileArnAutofetchHook;
    /// use axum::http::header;
    ///
    /// let headers = ProfileArnAutofetchHook::profile_headers("tok", "fp");
    /// assert_eq!(headers[header::AUTHORIZATION], "Bearer tok");
    /// ```
    pub fn profile_headers(token: &str, fingerprint: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let entries = [
            (header::AUTHORIZATION, format!("Bearer {token}")),
            (
                header::CONTENT_TYPE,
                "application/json; charset=UTF-8".to_string(),
            ),
            (
                HeaderName::from_static("content-encoding"),
                "amz-1.0".to_string(),
            ),
            (
                HeaderName::from_static("x-amz-target"),
                "AmazonCodeWhispererService.ListAvailableProfiles".to_string(),
            ),
            (
                header::USER_AGENT,
                format!("aws-sdk-js/1.0.27 KiroIDE-0.7.45-{fingerprint}"),
            ),
            (
                HeaderName::from_static("x-amz-user-agent"),
                format!("aws-sdk-js/1.0.27 KiroIDE-0.7.45-{fingerprint}"),
            ),
        ];
        for (name, value) in entries {
            if let Ok(value) = HeaderValue::from_str(&value) {
                headers.insert(name, value);
            }
        }
        headers
    }
    /// Extracts the first profile's ARN from a `ListAvailableProfiles` response, checking
    /// both the `arn` and `profileArn` field names (upstream has used both), and rejecting
    /// an empty string as "no usable profile".
    ///
    /// # Examples
    ///
    /// ```
    /// use lanius_core::compat::ProfileArnAutofetchHook;
    /// use serde_json::json;
    ///
    /// let response = json!({"profiles": [{"arn": "arn:aws:codewhisperer:..."}]});
    /// assert_eq!(
    ///     ProfileArnAutofetchHook::profile_from_response(&response),
    ///     Some("arn:aws:codewhisperer:...".to_string())
    /// );
    /// ```
    pub fn profile_from_response(value: &serde_json::Value) -> Option<String> {
        value
            .get("profiles")?
            .as_array()?
            .first()?
            .get("arn")
            .or_else(|| {
                value
                    .get("profiles")?
                    .as_array()?
                    .first()?
                    .get("profileArn")
            })?
            .as_str()
            .filter(|arn| !arn.is_empty())
            .map(ToOwned::to_owned)
    }
}
impl RequestHook for ProfileArnAutofetchHook {
    fn name(&self) -> &'static str {
        "profile_arn_autofetch"
    }
    fn apply(&self, _request: &mut CompatRequest) -> Result<()> {
        Ok(())
    }
}

/// Rewrites a `runtime.<region>.kiro.dev` host to its `q.<region>.amazonaws.com`
/// control-plane equivalent; hosts that don't match that pattern are returned unchanged.
///
/// # Examples
///
/// ```
/// use lanius_core::compat::control_plane_host;
///
/// assert_eq!(
///     control_plane_host("https://runtime.eu-west-1.kiro.dev"),
///     "https://q.eu-west-1.amazonaws.com"
/// );
/// ```
pub fn control_plane_host(raw_host: &str) -> String {
    runtime_to_q(raw_host).unwrap_or_else(|| raw_host.to_string())
}
/// Chooses the correct host for a chat/completion request: the paid runtime host requires
/// a non-empty profile ARN, so profile-less requests are transparently routed to the
/// control-plane host instead (see [`control_plane_host`]) rather than failing outright.
///
/// # Examples
///
/// ```
/// use lanius_core::compat::chat_host;
///
/// let runtime = "https://runtime.eu-west-1.kiro.dev";
/// assert_eq!(chat_host(runtime, Some("arn:aws:...")), runtime);
/// assert_eq!(chat_host(runtime, None), "https://q.eu-west-1.amazonaws.com");
/// ```
pub fn chat_host(raw_host: &str, profile_arn: Option<&str>) -> String {
    if profile_arn.is_none_or(str::is_empty) {
        control_plane_host(raw_host)
    } else {
        raw_host.to_string()
    }
}
fn runtime_to_q(raw_host: &str) -> Option<String> {
    let trimmed = raw_host.strip_suffix('/').unwrap_or(raw_host);
    let prefix = "https://runtime.";
    let suffix = ".kiro.dev";
    let region = trimmed.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (!region.is_empty() && !region.contains('/'))
        .then(|| format!("https://q.{region}.amazonaws.com"))
}
/// Forces specific control-plane-only operations (`ListAvailableModels`,
/// `GetUsageLimits`, and any `/mcp` path) to always target the AWS control-plane host,
/// regardless of which host the request was otherwise going to use — these operations are
/// not available on the paid runtime host at all.
struct ControlPlaneHostHook;
impl RequestHook for ControlPlaneHostHook {
    fn name(&self) -> &'static str {
        "control_plane_host"
    }
    fn apply(&self, request: &mut CompatRequest) -> Result<()> {
        if request.path.contains("ListAvailableModels")
            || request.path.contains("GetUsageLimits")
            || request.path.ends_with("/mcp")
        {
            if let Some(url) = &mut request.upstream_url {
                *url = control_plane_host(url);
            }
        }
        Ok(())
    }
}
/// Placeholder pipeline entry for the chat-host profile fallback. The real logic lives in
/// [`chat_host`] and is invoked directly by [`crate::auth::AuthManager::api_host`] before
/// this pipeline runs, so this hook is a no-op — kept for hook-name reporting parity, same
/// rationale as [`ToolNameAliasHook`].
struct ChatHostFallbackHook;
impl RequestHook for ChatHostFallbackHook {
    fn name(&self) -> &'static str {
        "chat_host_fallback"
    }
    fn apply(&self, _request: &mut CompatRequest) -> Result<()> {
        Ok(())
    }
}

/// Heuristically detects a Claude-branded client by scanning header *values* (excluding
/// `Authorization`, to avoid matching on token contents) for the substring `"claude"`
/// case-insensitively. Used to scope the `/v1/models` dashed-ID rewrite
/// ([`rewrite_model_ids`]) to clients that actually need it.
///
/// # Examples
///
/// ```
/// use lanius_core::compat::is_claude_client;
/// use axum::http::{HeaderMap, HeaderValue, header};
///
/// let mut headers = HeaderMap::new();
/// headers.insert(header::USER_AGENT, HeaderValue::from_static("claude-cli/1.0"));
/// assert!(is_claude_client(&headers));
/// ```
pub fn is_claude_client(headers: &HeaderMap) -> bool {
    headers.iter().any(|(name, value)| {
        name != header::AUTHORIZATION
            && value
                .to_str()
                .ok()
                .is_some_and(|value| value.to_ascii_lowercase().contains("claude"))
    })
}
/// Rewrites dotted Claude model IDs (e.g. `claude-sonnet-4.6`) in a `/v1/models` response
/// body to their dashed form (`claude-sonnet-4-6`), which some Claude-branded clients
/// require. Non-Claude model IDs, and any ID without both "claude" and a `.`, are left
/// untouched. If the body cannot be parsed as JSON or lacks a `data` array, or if nothing
/// actually changed, the original bytes are returned unmodified (avoiding an unnecessary
/// re-serialization when there is nothing to rewrite).
///
/// # Examples
///
/// ```
/// use lanius_core::compat::rewrite_model_ids;
///
/// let body = br#"{"data":[{"id":"claude-sonnet-4.6"}]}"#;
/// let rewritten = rewrite_model_ids(body);
/// assert!(String::from_utf8(rewritten).unwrap().contains("claude-sonnet-4-6"));
/// ```
pub fn rewrite_model_ids(body: &[u8]) -> Vec<u8> {
    let Ok(mut payload) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };
    let Some(items) = payload
        .get_mut("data")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return body.to_vec();
    };
    let mut changed = false;
    for item in items {
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(id) = id {
            if id.to_ascii_lowercase().contains("claude") && id.contains('.') {
                let dashed = id.replace('.', "-");
                if let Some(object) = item.as_object_mut() {
                    object.insert("id".to_string(), serde_json::Value::String(dashed));
                    changed = true;
                }
            }
        }
    }
    if changed {
        serde_json::to_vec(&payload).unwrap_or_else(|_| body.to_vec())
    } else {
        body.to_vec()
    }
}
/// Applies [`rewrite_model_ids`] to `GET /v1/models` responses, but only for requests that
/// [`is_claude_client`] identifies as Claude-branded, and only on a successful (`200 OK`)
/// response. Also refreshes `Content-Type`/`Content-Length` to stay consistent with the
/// (possibly resized) rewritten body.
struct ModelIdFormatHook;
impl ResponseHook for ModelIdFormatHook {
    fn name(&self) -> &'static str {
        "model_id_format"
    }
    fn apply(&self, request: &CompatRequest, response: &mut CompatResponse) -> Result<()> {
        if request.method == Method::GET
            && request.path == "/v1/models"
            && response.status == StatusCode::OK
            && is_claude_client(&request.headers)
        {
            response.body = rewrite_model_ids(&response.body);
            response.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            if let Ok(length) = HeaderValue::from_str(&response.body.len().to_string()) {
                response.headers.insert(header::CONTENT_LENGTH, length);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_name_alias_is_reversible_for_invalid_long_name_and_bracket_output() {
        let original = "mcp__plugin_everything-claude-code_github__create_pull_request_review";
        let mut aliases = ToolNameAliases::default();
        let alias = aliases.alias_for(original);
        assert!(alias.len() <= MAX_KIRO_TOOL_NAME_LENGTH);
        assert!(!ToolNameAliases::needs_alias(&alias));
        assert_eq!(aliases.original_for(&alias), original);
        let mut response = json!({"tool_calls":[{"function":{"name":alias}}],"text":format!("[Called {alias} with args: {{}}]")});
        aliases.restore_response_value(&mut response);
        assert_eq!(response["tool_calls"][0]["function"]["name"], original);
        assert!(
            response["text"]
                .as_str()
                .is_some_and(|text| text.contains(original))
        );
    }

    #[test]
    fn tool_name_alias_reserves_a_short_name_that_would_collide() {
        let original = "has spaces";
        let mut aliases = ToolNameAliases::default();
        let predicted = build_alias(original, 12);
        aliases.register_names([predicted.as_str(), original]);
        assert_ne!(aliases.alias_for(original), predicted);
    }

    #[test]
    fn profile_autofetch_claim_is_once_and_wire_helpers_are_exact() {
        let hook = ProfileArnAutofetchHook::default();
        assert!(hook.claim_fetch(true, false));
        assert!(!hook.claim_fetch(true, false));
        assert_eq!(
            ProfileArnAutofetchHook::profile_url("eu-west-1"),
            "https://q.eu-west-1.amazonaws.com/"
        );
        let headers = ProfileArnAutofetchHook::profile_headers("secret", "fp");
        assert_eq!(
            headers["x-amz-target"],
            "AmazonCodeWhispererService.ListAvailableProfiles"
        );
        assert_eq!(
            ProfileArnAutofetchHook::profile_from_response(
                &json!({"profiles":[{"profileArn":"arn"}]})
            ),
            Some("arn".into())
        );
    }

    #[test]
    fn host_hooks_keep_control_and_chat_decisions_separate() {
        let runtime = "https://runtime.eu-west-1.kiro.dev";
        assert_eq!(
            control_plane_host(runtime),
            "https://q.eu-west-1.amazonaws.com"
        );
        assert_eq!(
            chat_host(runtime, None),
            "https://q.eu-west-1.amazonaws.com"
        );
        assert_eq!(chat_host(runtime, Some("arn")), runtime);
        assert_eq!(
            control_plane_host("https://q.us-east-1.amazonaws.com"),
            "https://q.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn model_ids_only_rewrite_for_claude_clients_and_fix_length() {
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static("claude-cli"));
        let request = CompatRequest {
            method: Method::GET,
            path: "/v1/models".into(),
            headers,
            body: json!(null),
            upstream_url: None,
        };
        let mut response = CompatResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: br#"{"data":[{"id":"claude-sonnet-4.6"},{"id":"auto"}]}"#.to_vec(),
        };
        ModelIdFormatHook.apply(&request, &mut response).unwrap();
        assert_eq!(
            response.body,
            br#"{"data":[{"id":"claude-sonnet-4-6"},{"id":"auto"}]}"#
        );
        assert_eq!(
            response.headers[header::CONTENT_LENGTH],
            response.body.len().to_string()
        );
    }
}
