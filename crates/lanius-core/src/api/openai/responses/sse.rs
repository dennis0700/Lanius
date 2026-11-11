//! Kiro stream → OpenAI Responses API output.
//!
//! [`ResponseBuilder`] folds the upstream event stream (including executed
//! server-side web searches) into Responses output items, and — when
//! streaming — emits the matching semantic SSE events
//! (`response.created`, `response.output_item.added`,
//! `response.output_text.delta`, ..., `response.completed`). The
//! non-streaming path runs the same builder and returns the final
//! `response` object, so both paths produce identical output items.
//!
//! Item mapping: native thinking → `reasoning` item (as `summary_text`),
//! text → `message` item with one `output_text` part, executed searches →
//! `web_search_call` items, tool calls → `function_call` items (or
//! `custom_tool_call` for tools declared as `custom`). Text and reasoning
//! items are opened and closed as the upstream switches between them;
//! tool calls arrive complete and are emitted at the end.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value, json};

use super::super::sse::{OpenAiFormatContext, save_truncations, usage_value};
use crate::api::search_loop::LoopEvent;
use crate::convert::{TOOL_SEARCH_TOOL_NAME, ToolNamespaces};
use crate::error::{GatewayError, Result};
use crate::tokenizer::count_tokens;
use crate::truncation::is_content_truncated;
use crate::upstream::{
    KiroEvent, KiroEventType, ToolCall, WebSearchCall, deduplicate_tool_calls,
    parse_bracket_tool_calls,
};
use crate::utils::{format_json_spaced, generate_tool_call_id};

/// Per-request context for building a Responses API response.
#[derive(Clone)]
pub(crate) struct ResponsesContext {
    /// Model name, token-accounting inputs, truncation store and tool name
    /// aliases, shared with the Chat Completions formatter.
    pub(crate) format: OpenAiFormatContext,
    /// Original names of tools declared as `custom` (freeform).
    pub(crate) custom_tools: HashSet<String>,
    /// `(namespace, name)` of tools declared inside `namespace` tools, by flattened name.
    pub(crate) tool_namespaces: ToolNamespaces,
    /// Whether a client-executed `tool_search` tool was declared, so calls to
    /// [`TOOL_SEARCH_TOOL_NAME`] become `tool_search_call` items.
    pub(crate) tool_search: bool,
    /// Request parameters echoed on every `response` object.
    pub(crate) echo: Map<String, Value>,
}

/// Encodes a [`crate::api::search_loop::SearchLoop`] stream as Responses
/// API SSE frames (`event: <type>\ndata: <json>\n\n`). A mid-stream
/// upstream failure closes any open item and ends with `response.failed`.
pub(crate) fn encode_responses_sse<S>(
    events: S,
    context: ResponsesContext,
) -> impl Stream<Item = Result<String>> + Send
where
    S: Stream<Item = Result<LoopEvent>> + Send + 'static,
{
    async_stream::stream! {
        let mut builder = ResponseBuilder::new(context, true);
        builder.start();
        for frame in builder.take_frames() {
            yield Ok(frame);
        }
        futures_util::pin_mut!(events);
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => builder.push(event),
                Err(error) => {
                    builder.fail(&error);
                    for frame in builder.take_frames() {
                        yield Ok(frame);
                    }
                    return;
                }
            }
            for frame in builder.take_frames() {
                yield Ok(frame);
            }
        }
        builder.finish();
        for frame in builder.take_frames() {
            yield Ok(frame);
        }
    }
}

/// Drains a [`crate::api::search_loop::SearchLoop`] stream into a single
/// non-streaming Responses API `response` object.
pub(crate) async fn collect_response<S>(events: S, context: ResponsesContext) -> Result<Value>
where
    S: Stream<Item = Result<LoopEvent>>,
{
    let mut builder = ResponseBuilder::new(context, false);
    futures_util::pin_mut!(events);
    while let Some(event) = events.next().await {
        builder.push(event?);
    }
    Ok(builder.finish())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Reasoning,
    Message,
}

/// The reasoning or message item currently receiving deltas.
struct OpenItem {
    kind: ItemKind,
    id: String,
    text: String,
}

struct ResponseBuilder {
    context: ResponsesContext,
    streaming: bool,
    id: String,
    created_at: i64,
    sequence: u64,
    frames: Vec<String>,
    output: Vec<Value>,
    open: Option<OpenItem>,
    /// Raw (still aliased) upstream text, for bracket tool calls,
    /// truncation detection and token counting.
    full_content: String,
    thinking: String,
    alias_pending: String,
    tool_calls: Vec<ToolCall>,
    metering: Option<Value>,
    received_usage: bool,
    context_usage: Option<f64>,
}

impl ResponseBuilder {
    fn new(context: ResponsesContext, streaming: bool) -> Self {
        Self {
            context,
            streaming,
            id: new_id("resp"),
            created_at: unix_timestamp(),
            sequence: 0,
            frames: Vec::new(),
            output: Vec::new(),
            open: None,
            full_content: String::new(),
            thinking: String::new(),
            alias_pending: String::new(),
            tool_calls: Vec::new(),
            metering: None,
            received_usage: false,
            context_usage: None,
        }
    }

    fn take_frames(&mut self) -> Vec<String> {
        std::mem::take(&mut self.frames)
    }

    /// Queues one SSE event (streaming only), stamping `type` and
    /// `sequence_number`.
    fn emit(&mut self, kind: &str, body: Value) {
        if !self.streaming {
            return;
        }
        let mut body = match body {
            Value::Object(body) => body,
            _ => Map::new(),
        };
        body.insert("type".into(), Value::String(kind.into()));
        body.insert("sequence_number".into(), json!(self.sequence));
        self.sequence += 1;
        self.frames.push(format!(
            "event: {kind}\ndata: {}\n\n",
            format_json_spaced(&Value::Object(body))
        ));
    }

    /// The `response` object in its current state.
    fn response(&self, status: &str) -> Value {
        let mut response = self.context.echo.clone();
        response.insert("id".into(), Value::String(self.id.clone()));
        response.insert("object".into(), json!("response"));
        response.insert("created_at".into(), json!(self.created_at));
        response.insert("status".into(), json!(status));
        response.insert("model".into(), json!(self.context.format.model));
        response.insert("output".into(), Value::Array(self.output.clone()));
        response.insert("error".into(), Value::Null);
        response.insert("incomplete_details".into(), Value::Null);
        response.insert("usage".into(), Value::Null);
        Value::Object(response)
    }

    fn start(&mut self) {
        let response = self.response("in_progress");
        self.emit("response.created", json!({"response": response}));
        self.emit("response.in_progress", json!({"response": response}));
    }

    fn push(&mut self, event: LoopEvent) {
        let event = match event {
            LoopEvent::WebSearch(call) => return self.on_search(&call),
            LoopEvent::Kiro(event) => event,
        };
        match event {
            KiroEvent {
                event_type: KiroEventType::Content,
                content: Some(content),
                ..
            } if !content.is_empty() => self.on_content(&content),
            KiroEvent {
                event_type: KiroEventType::Thinking,
                thinking_content: Some(thinking),
                ..
            } if !thinking.is_empty() => self.on_thinking(&thinking),
            KiroEvent {
                event_type: KiroEventType::ToolUse,
                tool_use: Some(tool),
                ..
            } => self.tool_calls.push(tool),
            KiroEvent {
                event_type: KiroEventType::Usage,
                usage,
                ..
            } => {
                self.received_usage = usage.is_some();
                self.metering = usage;
            }
            KiroEvent {
                event_type: KiroEventType::ContextUsage,
                context_usage_percentage: Some(percentage),
                ..
            } => self.context_usage = Some(percentage),
            _ => {}
        }
    }

    fn on_content(&mut self, content: &str) {
        self.full_content.push_str(content);
        // Partial tool-name aliases are held back until complete.
        let text = self.context.format.tool_name_aliases.restore_text_fragment(
            &mut self.alias_pending,
            content,
            false,
        );
        if !text.is_empty() {
            self.ensure_open(ItemKind::Message);
            self.append(&text);
        }
    }

    fn on_thinking(&mut self, thinking: &str) {
        self.thinking.push_str(thinking);
        self.flush_text();
        self.ensure_open(ItemKind::Reasoning);
        self.append(thinking);
    }

    fn on_search(&mut self, call: &WebSearchCall) {
        self.flush_text();
        self.close_open();
        let id = new_id("ws");
        let output_index = self.output.len();
        let status = if call.outcome.is_ok() {
            "completed"
        } else {
            "failed"
        };
        let action = json!({"type": "search", "query": call.query});
        self.emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": {
                "id": id, "type": "web_search_call", "status": "in_progress", "action": action,
            }}),
        );
        self.emit(
            "response.web_search_call.completed",
            json!({"item_id": id, "output_index": output_index}),
        );
        let item = json!({"id": id, "type": "web_search_call", "status": status, "action": action});
        self.emit(
            "response.output_item.done",
            json!({"output_index": output_index, "item": item}),
        );
        self.output.push(item);
    }

    /// Releases text still held back for alias restoration.
    fn flush_text(&mut self) {
        let text = self.context.format.tool_name_aliases.restore_text_fragment(
            &mut self.alias_pending,
            "",
            true,
        );
        if !text.is_empty() {
            self.ensure_open(ItemKind::Message);
            self.append(&text);
        }
    }

    fn ensure_open(&mut self, kind: ItemKind) {
        if self.open.as_ref().is_some_and(|open| open.kind == kind) {
            return;
        }
        self.close_open();
        let output_index = self.output.len();
        let (id, item, part_event, part) = match kind {
            ItemKind::Message => {
                let id = new_id("msg");
                let item = json!({
                    "id": id, "type": "message", "status": "in_progress",
                    "role": "assistant", "content": [],
                });
                (
                    id,
                    item,
                    "response.content_part.added",
                    output_text_part(""),
                )
            }
            ItemKind::Reasoning => {
                let id = new_id("rs");
                let item = json!({"id": id, "type": "reasoning", "summary": []});
                (
                    id,
                    item,
                    "response.reasoning_summary_part.added",
                    summary_part(""),
                )
            }
        };
        self.emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": item}),
        );
        let mut body = part_location(kind, &id, output_index);
        body["part"] = part;
        self.emit(part_event, body);
        self.open = Some(OpenItem {
            kind,
            id,
            text: String::new(),
        });
    }

    fn append(&mut self, delta: &str) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        open.text.push_str(delta);
        let kind = open.kind;
        let mut body = part_location(kind, &open.id, self.output.len());
        body["delta"] = json!(delta);
        let event = match kind {
            ItemKind::Message => {
                body["logprobs"] = json!([]);
                "response.output_text.delta"
            }
            ItemKind::Reasoning => "response.reasoning_summary_text.delta",
        };
        self.emit(event, body);
    }

    fn close_open(&mut self) {
        let Some(OpenItem { kind, id, text }) = self.open.take() else {
            return;
        };
        let output_index = self.output.len();
        let mut text_done = part_location(kind, &id, output_index);
        text_done["text"] = json!(text);
        let mut part_done = part_location(kind, &id, output_index);
        let item = match kind {
            ItemKind::Message => {
                text_done["logprobs"] = json!([]);
                self.emit("response.output_text.done", text_done);
                part_done["part"] = output_text_part(&text);
                self.emit("response.content_part.done", part_done);
                json!({
                    "id": id, "type": "message", "status": "completed", "role": "assistant",
                    "content": [output_text_part(&text)],
                })
            }
            ItemKind::Reasoning => {
                self.emit("response.reasoning_summary_text.done", text_done);
                part_done["part"] = summary_part(&text);
                self.emit("response.reasoning_summary_part.done", part_done);
                json!({"id": id, "type": "reasoning", "summary": [summary_part(&text)]})
            }
        };
        self.emit(
            "response.output_item.done",
            json!({"output_index": output_index, "item": item}),
        );
        self.output.push(item);
    }

    fn push_tool_call(&mut self, call: ToolCall) {
        let name = self
            .context
            .format
            .tool_name_aliases
            .original_for(&call.name);
        let call_id = call
            .id
            .filter(|id| !id.is_empty())
            .unwrap_or_else(generate_tool_call_id);
        let output_index = self.output.len();
        if self.context.tool_search && name == TOOL_SEARCH_TOOL_NAME {
            return self.push_tool_search_call(call_id, &call.arguments);
        }
        let custom = self.context.custom_tools.contains(&name);
        let (id, kind, field, value, delta_event, done_event) = if custom {
            let input = custom_tool_input(&call.arguments);
            (
                new_id("ctc"),
                "custom_tool_call",
                "input",
                input,
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.done",
            )
        } else {
            (
                new_id("fc"),
                "function_call",
                "arguments",
                call.arguments,
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
            )
        };
        let mut item = json!({
            "id": id, "type": kind, "status": "in_progress", "call_id": call_id, "name": name,
        });
        if let Some((namespace, bare)) = self.context.tool_namespaces.get(&name) {
            item["name"] = json!(bare);
            item["namespace"] = json!(namespace);
        }
        item[field] = json!("");
        self.emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": item}),
        );
        self.emit(
            delta_event,
            json!({"item_id": id, "output_index": output_index, "delta": value}),
        );
        let mut done = json!({"item_id": id, "output_index": output_index});
        done[field] = json!(value);
        self.emit(done_event, done);
        item[field] = Value::String(value);
        item["status"] = json!("completed");
        self.emit(
            "response.output_item.done",
            json!({"output_index": output_index, "item": item}),
        );
        self.output.push(item);
    }

    /// Emits a client-executed `tool_search_call` item; its `arguments` are a JSON
    /// object rather than a JSON string.
    fn push_tool_search_call(&mut self, call_id: String, arguments: &str) {
        let output_index = self.output.len();
        let arguments = serde_json::from_str::<Value>(arguments)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let mut item = json!({
            "id": new_id("tsc"), "type": "tool_search_call", "status": "in_progress",
            "execution": "client", "call_id": call_id, "arguments": arguments,
        });
        self.emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": item}),
        );
        item["status"] = json!("completed");
        self.emit(
            "response.output_item.done",
            json!({"output_index": output_index, "item": item}),
        );
        self.output.push(item);
    }

    /// Closes all items, emits tool calls and the terminal event, and
    /// returns the final `response` object.
    fn finish(&mut self) -> Value {
        self.flush_text();
        self.close_open();
        let mut calls = std::mem::take(&mut self.tool_calls);
        calls.extend(parse_bracket_tool_calls(&self.full_content));
        let calls = deduplicate_tool_calls(calls);
        let truncated = is_content_truncated(
            self.received_usage || self.context_usage.is_some(),
            &self.full_content,
            !calls.is_empty(),
        );
        save_truncations(&self.context.format, &calls, truncated, &self.full_content);
        for call in calls {
            self.push_tool_call(call);
        }

        let mut response = self.response(if truncated { "incomplete" } else { "completed" });
        response["usage"] = self.usage();
        if truncated {
            response["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }
        let event = if truncated {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.emit(event, json!({"response": response}));
        response
    }

    fn fail(&mut self, error: &GatewayError) {
        error.report("POST /v1/responses stream");
        self.flush_text();
        self.close_open();
        let mut response = self.response("failed");
        response["error"] = super::routes::openai_error(error);
        self.emit("response.failed", json!({"response": response}));
    }

    /// Responses-shaped usage, derived from the Chat Completions usage
    /// computation so both APIs count tokens identically.
    fn usage(&self) -> Value {
        let chat = usage_value(
            &self.context.format,
            &self.full_content,
            &self.thinking,
            self.context_usage,
            self.metering.as_ref(),
        );
        let mut usage = json!({
            "input_tokens": chat["prompt_tokens"],
            "input_tokens_details": {
                "cached_tokens": chat
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .cloned()
                    .unwrap_or(json!(0)),
            },
            "output_tokens": chat["completion_tokens"],
            "output_tokens_details": {"reasoning_tokens": count_tokens(&self.thinking, true)},
            "total_tokens": chat["total_tokens"],
        });
        if let Some(credits) = chat.get("credits_used") {
            usage["credits_used"] = credits.clone();
        }
        usage
    }
}

/// The `item_id`/`output_index`/part-index fields shared by part-level
/// events of a message (`content_index`) or reasoning (`summary_index`) item.
fn part_location(kind: ItemKind, id: &str, output_index: usize) -> Value {
    let index_field = match kind {
        ItemKind::Message => "content_index",
        ItemKind::Reasoning => "summary_index",
    };
    let mut location = json!({"item_id": id, "output_index": output_index});
    location[index_field] = json!(0);
    location
}

fn output_text_part(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": [], "logprobs": []})
}

fn summary_part(text: &str) -> Value {
    json!({"type": "summary_text", "text": text})
}

/// The freeform `input` of a `custom` tool call: the `input` string Kiro
/// was asked to pass, falling back to the raw arguments.
fn custom_tool_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("input")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| arguments.to_owned())
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::stream;

    use super::*;
    use crate::compat::ToolNameAliases;
    use crate::config::Config;
    use crate::model::ModelInfoCache;
    use crate::truncation::TruncationStore;
    use crate::upstream::{WebSearchError, WebSearchResponse};

    fn context(aliases: ToolNameAliases) -> ResponsesContext {
        let format = OpenAiFormatContext::new(
            "m",
            ModelInfoCache::default(),
            vec![json!({"role": "user", "content": "hello"})],
            None,
            Arc::new(Config::default()),
            "conversation",
            TruncationStore::default(),
        )
        .with_tool_name_aliases(aliases);
        let mut echo = Map::new();
        echo.insert("tool_choice".into(), json!("auto"));
        ResponsesContext {
            format,
            custom_tools: HashSet::from(["apply_patch".to_owned()]),
            tool_namespaces: ToolNamespaces::from([(
                "collaboration__spawn_agent".to_owned(),
                ("collaboration".to_owned(), "spawn_agent".to_owned()),
            )]),
            tool_search: true,
            echo,
        }
    }

    fn kiro(event: KiroEvent) -> Result<LoopEvent> {
        Ok(LoopEvent::Kiro(event))
    }

    fn tool(id: &str, name: &str, arguments: &str) -> KiroEvent {
        KiroEvent::tool_use(ToolCall {
            id: Some(id.into()),
            name: name.into(),
            arguments: arguments.into(),
            truncation: None,
        })
    }

    async fn sse(events: Vec<Result<LoopEvent>>) -> Vec<(String, Value)> {
        encode_responses_sse(stream::iter(events), context(ToolNameAliases::default()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|frame| {
                let frame = frame.unwrap_or_else(|error| panic!("frame: {error}"));
                let (head, data) = frame
                    .strip_suffix("\n\n")
                    .and_then(|frame| frame.split_once('\n'))
                    .unwrap_or_else(|| panic!("malformed frame {frame:?}"));
                let event = head.strip_prefix("event: ").unwrap_or_default().to_owned();
                let data = data.strip_prefix("data: ").unwrap_or_default();
                let data: Value =
                    serde_json::from_str(data).unwrap_or_else(|error| panic!("json: {error}"));
                assert_eq!(data["type"], event.as_str());
                (event, data)
            })
            .collect()
    }

    #[tokio::test]
    async fn stream_emits_reasoning_message_and_tool_items_in_order() {
        let frames = sse(vec![
            kiro(KiroEvent::thinking(Some("think".into()), None)),
            kiro(KiroEvent::content("Hel")),
            kiro(KiroEvent::content("lo")),
            kiro(tool("call_1", "shell", r#"{"cmd": "ls"}"#)),
            kiro(tool(
                "call_2",
                "apply_patch",
                r#"{"input": "*** Begin Patch"}"#,
            )),
            kiro(KiroEvent::usage(json!({"usage": 0.5}))),
        ])
        .await;
        let names: Vec<&str> = frames.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let sequence: Vec<u64> = frames
            .iter()
            .filter_map(|(_, data)| data["sequence_number"].as_u64())
            .collect();
        assert_eq!(sequence, (0..frames.len() as u64).collect::<Vec<_>>());
        assert_eq!(frames[0].1["response"]["status"], "in_progress");
        assert_eq!(frames[0].1["response"]["tool_choice"], "auto");
        assert_eq!(frames[12].1["text"], "Hello");
        assert_eq!(frames[12].1["output_index"], 1);

        let response = &frames[23].1["response"];
        assert_eq!(response["status"], "completed");
        let output = response["output"].as_array().cloned().unwrap_or_default();
        assert_eq!(output.len(), 4);
        assert_eq!(output[0]["summary"][0]["text"], "think");
        assert_eq!(output[1]["content"][0]["text"], "Hello");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[2]["call_id"], "call_1");
        assert_eq!(output[2]["arguments"], r#"{"cmd": "ls"}"#);
        assert_eq!(output[3]["type"], "custom_tool_call");
        assert_eq!(output[3]["input"], "*** Begin Patch");
        assert_eq!(response["usage"]["credits_used"], json!(0.5));
        assert!(response["usage"]["output_tokens_details"]["reasoning_tokens"].as_u64() > Some(0));
    }

    #[tokio::test]
    async fn web_search_splits_message_items_and_missing_usage_is_incomplete() {
        let frames = sse(vec![
            kiro(KiroEvent::content("Let me search.")),
            Ok(LoopEvent::WebSearch(WebSearchCall {
                tool_use_id: "t".into(),
                query: "rust".into(),
                outcome: Ok(WebSearchResponse::default()),
            })),
            Ok(LoopEvent::WebSearch(WebSearchCall {
                tool_use_id: "u".into(),
                query: "".into(),
                outcome: Err(WebSearchError {
                    code: "invalid_tool_input",
                    message: "empty".into(),
                }),
            })),
            kiro(KiroEvent::content("Found it.")),
        ])
        .await;
        let (event, last) = frames.last().cloned().unwrap_or_default();
        assert_eq!(event, "response.incomplete");
        let response = &last["response"];
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        let kinds: Vec<&str> = response["output"]
            .as_array()
            .map(|output| {
                output
                    .iter()
                    .filter_map(|item| item["type"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            kinds,
            ["message", "web_search_call", "web_search_call", "message"]
        );
        assert_eq!(response["output"][1]["action"]["query"], "rust");
        assert_eq!(response["output"][2]["status"], "failed");
    }

    #[tokio::test]
    async fn namespaced_tool_calls_restore_namespace() {
        let frames = sse(vec![
            kiro(tool("call_1", "collaboration__spawn_agent", "{}")),
            kiro(tool("call_2", "wait", "{}")),
            kiro(KiroEvent::usage(json!({}))),
        ])
        .await;
        let (_, last) = frames.last().cloned().unwrap_or_default();
        let output = &last["response"]["output"];
        assert_eq!(output[0]["name"], "spawn_agent");
        assert_eq!(output[0]["namespace"], "collaboration");
        assert_eq!(output[1]["name"], "wait");
        assert!(output[1].get("namespace").is_none());
    }

    #[tokio::test]
    async fn tool_search_calls_become_tool_search_call_items() {
        let frames = sse(vec![
            kiro(tool(
                "call_s",
                "tool_search",
                r#"{"query": "echo", "limit": 3}"#,
            )),
            kiro(KiroEvent::usage(json!({}))),
        ])
        .await;
        let (_, last) = frames.last().cloned().unwrap_or_default();
        let item = &last["response"]["output"][0];
        assert_eq!(item["type"], "tool_search_call");
        assert_eq!(item["execution"], "client");
        assert_eq!(item["call_id"], "call_s");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["arguments"], json!({"query": "echo", "limit": 3}));
        let done: Vec<_> = frames
            .iter()
            .filter(|(name, _)| name == "response.output_item.done")
            .collect();
        assert_eq!(done.len(), 1);
    }

    #[tokio::test]
    async fn context_limit_failure_reports_openai_code() {
        let error = GatewayError::Upstream {
            status: 400,
            info: Box::new(crate::error::enhance_kiro_error(
                &json!({"reason": "CONTENT_LENGTH_EXCEEDS_THRESHOLD"}),
            )),
        };
        let frames = sse(vec![kiro(KiroEvent::content("x")), Err(error)]).await;
        let (event, last) = frames.last().cloned().unwrap_or_default();
        assert_eq!(event, "response.failed");
        assert_eq!(last["response"]["error"]["code"], "context_length_exceeded");
    }

    #[tokio::test]
    async fn mid_stream_error_closes_items_and_fails_response() {
        let frames = sse(vec![
            kiro(KiroEvent::content("partial")),
            Err(GatewayError::StreamReadTimeout(Duration::from_secs(1))),
        ])
        .await;
        let names: Vec<&str> = frames.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names[names.len() - 2], "response.output_item.done");
        let (event, last) = frames.last().cloned().unwrap_or_default();
        assert_eq!(event, "response.failed");
        assert_eq!(last["response"]["status"], "failed");
        assert!(last["response"]["error"]["message"].is_string());
        assert_eq!(
            last["response"]["output"][0]["content"][0]["text"],
            "partial"
        );
    }

    #[tokio::test]
    async fn collected_response_restores_aliases() {
        let original = "mcp__plugin_everything_claude_code_github__create_pull_request_review";
        let mut aliases = ToolNameAliases::default();
        let alias = aliases.alias_for(original);
        let split = alias.chars().count() / 2;
        let events = vec![
            kiro(KiroEvent::content(format!(
                "[Called {}",
                alias.chars().take(split).collect::<String>()
            ))),
            kiro(KiroEvent::content(format!(
                "{} with args: {{}}]",
                alias.chars().skip(split).collect::<String>()
            ))),
            kiro(tool("call", &alias, "{}")),
            kiro(KiroEvent::usage(json!({}))),
        ];
        let response = collect_response(stream::iter(events), context(aliases))
            .await
            .unwrap_or_else(|error| panic!("collect: {error}"));
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        let text = response["output"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains(original) && !text.contains(&alias));
        // The bracket marker and the structured call dedupe to one item.
        assert_eq!(response["output"].as_array().map(Vec::len), Some(2));
        assert_eq!(response["output"][1]["name"], original);
        assert!(
            response["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("resp_"))
        );
    }
}
