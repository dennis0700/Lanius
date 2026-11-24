//! Server-side execution of the model-facing `web_search` tool.
//!
//! Kiro's models can call `web_search`, but, as in the Kiro CLI, the caller
//! has to run the search and hand the results back. [`SearchLoop::events`]
//! wraps the upstream event stream of a chat request: content and thinking
//! are forwarded as they arrive, and when a round ends with only
//! `web_search` calls, Lanius runs them through Kiro's native search (see
//! [`KiroHttpClient::web_search`]), appends the call and its results to the
//! conversation, and continues with a new upstream request. Clients only see
//! the final answer, plus a [`LoopEvent::WebSearch`] per executed search
//! that the Anthropic API renders as `server_tool_use` +
//! `web_search_tool_result` blocks.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt, stream};
use serde_json::{Map, Value, json};

use crate::auth::AuthManager;
use crate::config::Config;
use crate::error::{GatewayError, Result};
use crate::upstream::{
    KiroEvent, KiroEventType, KiroHttpClient, StreamResult, ToolCall, WebSearchCall,
    WebSearchError, parse_kiro_stream,
};
use crate::utils::{generate_continuation_id, generate_tool_call_id};

/// Name of the model-facing search tool that Lanius executes itself.
pub(crate) const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// Searches allowed per request when the client did not set a limit.
pub(crate) const DEFAULT_MAX_SEARCHES: usize = 5;

/// A raw upstream response body.
pub(crate) type ByteStream = BoxStream<'static, std::result::Result<Bytes, reqwest::Error>>;

/// One item of a [`SearchLoop`] stream.
pub(crate) enum LoopEvent {
    /// An upstream event to forward to the client.
    Kiro(KiroEvent),
    /// A search Lanius executed on the model's behalf.
    WebSearch(WebSearchCall),
}

/// Runs `web_search` calls on the model's behalf across upstream rounds.
#[derive(Clone)]
pub(crate) struct SearchLoop {
    pub(crate) auth: Arc<AuthManager>,
    pub(crate) config: Arc<Config>,
    /// Search budget for the request; `None` disables interception, so
    /// every tool call reaches the client unchanged.
    pub(crate) max_searches: Option<usize>,
}

/// A search executed in one round, with the input the model sent.
struct ExecutedSearch {
    call: WebSearchCall,
    input: Value,
}

impl SearchLoop {
    /// Turns the first upstream response (`first`, produced from `payload`)
    /// into a stream of events spanning every search round.
    ///
    /// A round ending with only `web_search` calls is continued; a round
    /// that also calls client tools ends the loop and those client tool
    /// calls are forwarded (the `web_search` calls in it are dropped, since
    /// the client cannot answer them). Once the budget is spent the model
    /// gets `max_uses_exceeded` results, and one round later the loop stops.
    pub(crate) fn events(
        self,
        first: ByteStream,
        payload: Value,
    ) -> impl Stream<Item = Result<LoopEvent>> + Send + 'static {
        let this = self;
        async_stream::try_stream! {
            let mut source = first;
            let mut payload = payload;
            let max = this.max_searches.unwrap_or_default();
            let mut used = 0;
            let mut rounds = 0;
            loop {
                let events = parse_kiro_stream(
                    source,
                    this.config.first_token_timeout,
                    this.config.streaming_read_timeout,
                );
                futures_util::pin_mut!(events);
                let mut round_text = String::new();
                let mut round_reasoning = RoundReasoning::default();
                let mut calls = Vec::new();
                while let Some(event) = events.next().await {
                    let event = event?;
                    match event.event_type {
                        KiroEventType::ToolUse if this.max_searches.is_some() => {
                            calls.extend(event.tool_use);
                            continue;
                        }
                        KiroEventType::Content => {
                            round_text.push_str(event.content.as_deref().unwrap_or_default());
                        }
                        KiroEventType::Thinking => round_reasoning.apply(&event),
                        _ => {}
                    }
                    yield LoopEvent::Kiro(event);
                }

                let (searches, others): (Vec<_>, Vec<_>) = calls
                    .into_iter()
                    .partition(|call| call.name == WEB_SEARCH_TOOL_NAME);
                if searches.is_empty() || !others.is_empty() || rounds > max {
                    if !searches.is_empty() {
                        tracing::warn!(
                            dropped = searches.len(),
                            rounds,
                            "ending web_search loop with unanswered web_search calls"
                        );
                    }
                    for call in others {
                        yield LoopEvent::Kiro(KiroEvent::tool_use(call));
                    }
                    break;
                }

                rounds += 1;
                let client = KiroHttpClient::new(this.auth.clone(), &this.config)?;
                let mut executed = Vec::with_capacity(searches.len());
                for call in searches {
                    let search = run_search(&client, call, &mut used, max).await;
                    yield LoopEvent::WebSearch(search.call.clone());
                    executed.push(search);
                }
                payload = continuation_payload(
                    payload,
                    &round_text,
                    round_reasoning.into_value(),
                    &executed,
                )?;
                source = open_chat_stream(this.auth.clone(), &this.config, &payload).await?;
            }
        }
    }
}

/// Native reasoning from one search round, replayed on the round's assistant
/// turn so the model continues from it, as the Kiro CLI does.
#[derive(Default)]
struct RoundReasoning {
    text: String,
    signature: Option<String>,
    redacted: Option<String>,
}

impl RoundReasoning {
    fn apply(&mut self, event: &KiroEvent) {
        if let Some(text) = &event.thinking_content {
            self.text.push_str(text);
        }
        if let Some(signature) = &event.thinking_signature {
            self.signature = Some(signature.clone());
        }
        if let Some(redacted) = &event.redacted_thinking {
            self.redacted = Some(redacted.clone());
        }
    }

    /// The round's `reasoningContent`, if Kiro signed (or redacted) it. Kiro
    /// sends hidden reasoning as a signature with no text, which goes back
    /// with its `"..."` placeholder text.
    fn into_value(self) -> Option<Value> {
        if let Some(signature) = self.signature.filter(|signature| !signature.is_empty()) {
            let text = if self.text.is_empty() {
                "...".to_owned()
            } else {
                self.text
            };
            return Some(json!({"reasoningText": {"text": text, "signature": signature}}));
        }
        self.redacted
            .filter(|data| !data.is_empty())
            .map(|data| json!({"redactedContent": data}))
    }
}

/// Inserts a paragraph break between text written before a search and text
/// written after it, so text-only renderings of a multi-round answer do not
/// run sentences together.
#[derive(Default)]
struct RoundSeparator {
    wrote_text: bool,
    searched: bool,
}

impl RoundSeparator {
    fn on_search(&mut self) {
        self.searched = self.wrote_text;
    }

    /// Returns the separator to emit before `event`, if any.
    fn before(&mut self, event: &KiroEvent) -> Option<KiroEvent> {
        let is_text = event.event_type == KiroEventType::Content
            && event
                .content
                .as_deref()
                .is_some_and(|text| !text.is_empty());
        if !is_text {
            return None;
        }
        self.wrote_text = true;
        std::mem::take(&mut self.searched).then(|| KiroEvent::content("\n\n"))
    }
}

/// Reduces a [`SearchLoop`] stream to plain upstream events for clients
/// that only render text (OpenAI): executed searches are dropped and text
/// from different rounds is separated.
pub(crate) fn text_events(
    events: impl Stream<Item = Result<LoopEvent>> + Send + 'static,
) -> impl Stream<Item = Result<KiroEvent>> + Send + 'static {
    async_stream::try_stream! {
        futures_util::pin_mut!(events);
        let mut separator = RoundSeparator::default();
        while let Some(event) = events.next().await {
            match event? {
                LoopEvent::WebSearch(_) => separator.on_search(),
                LoopEvent::Kiro(event) => {
                    if let Some(gap) = separator.before(&event) {
                        yield gap;
                    }
                    yield event;
                }
            }
        }
    }
}

/// Drains a [`SearchLoop`] stream into a [`StreamResult`] plus the searches
/// that were executed, for non-streaming responses. Text from different
/// rounds is separated as in [`text_events`].
pub(crate) async fn collect(
    events: impl Stream<Item = Result<LoopEvent>>,
) -> Result<(StreamResult, Vec<WebSearchCall>)> {
    futures_util::pin_mut!(events);
    let mut result = StreamResult::new();
    let mut searches = Vec::new();
    let mut separator = RoundSeparator::default();
    while let Some(event) = events.next().await {
        match event? {
            LoopEvent::WebSearch(call) => {
                separator.on_search();
                searches.push(call);
            }
            LoopEvent::Kiro(event) => {
                if let Some(gap) = separator.before(&event) {
                    result.apply(gap);
                }
                result.apply(event);
            }
        }
    }
    result.finalize();
    Ok((result, searches))
}

/// Issues a `generateAssistantResponse` request for `payload`, preflighted
/// with [`preflight_first_byte`].
pub(crate) async fn open_chat_stream(
    auth: Arc<AuthManager>,
    config: &Config,
    payload: &Value,
) -> Result<ByteStream> {
    preflight_first_byte(
        config.first_token_max_retries,
        config.first_token_timeout,
        || {
            let auth = auth.clone();
            async move {
                let client = KiroHttpClient::new(auth, config)?;
                let response = client.chat_request_with_retry(payload, true).await?;
                Ok(response.bytes_stream().boxed())
            }
        },
    )
    .await
}

/// Calls `request` (a factory, so each retry issues a fresh HTTP request)
/// up to `retries` times, waiting up to `timeout` for the first byte of
/// each attempt. Waiting for the first byte makes connection-level failures
/// (auth, rate limits, context limit) surface as an `Err` before any bytes
/// reach the client. The first byte is re-prepended to the returned stream
/// so no data is lost; a transient network error on it is retried too.
pub(crate) async fn preflight_first_byte<F, Fut>(
    retries: u32,
    timeout: std::time::Duration,
    mut request: F,
) -> Result<ByteStream>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<ByteStream>>,
{
    let tries = retries.max(1);
    for attempt in 0..tries {
        let mut source = request().await?;
        match tokio::time::timeout(timeout, source.next()).await {
            Ok(Some(Ok(first))) => {
                return Ok(
                    stream::once(async move { Ok::<Bytes, reqwest::Error>(first) })
                        .chain(source)
                        .boxed(),
                );
            }
            Ok(Some(Err(error))) => {
                // Nothing has been sent to the client yet, so a transient
                // failure on the first chunk can be retried with a fresh request.
                match GatewayError::from(error) {
                    GatewayError::Network(info) if info.is_retryable && attempt + 1 < tries => {
                        tracing::warn!(
                            attempt = attempt + 1,
                            category = %info.category,
                            details = %info.technical_details,
                            "first chunk read failed; retrying upstream request"
                        );
                        continue;
                    }
                    error => return Err(error),
                }
            }
            Ok(None) => return Ok(source),
            Err(_) if attempt + 1 < tries => {
                tracing::warn!(
                    attempt = attempt + 1,
                    timeout = ?timeout,
                    "first token timed out; retrying upstream request"
                );
                continue;
            }
            Err(_) => return Err(GatewayError::FirstTokenTimeout(timeout)),
        }
    }
    Err(GatewayError::Internal(
        "first-token preflight exited unexpectedly".into(),
    ))
}

/// Executes one `web_search` call. Failures become an error outcome that
/// is shown to the model instead of failing the whole request.
async fn run_search(
    client: &KiroHttpClient,
    call: ToolCall,
    used: &mut usize,
    max: usize,
) -> ExecutedSearch {
    let input = serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let outcome = if query.is_empty() {
        Err(WebSearchError {
            code: "invalid_tool_input",
            message: "web_search requires a non-empty `query`".into(),
        })
    } else if *used >= max {
        Err(WebSearchError {
            code: "max_uses_exceeded",
            message: format!(
                "web_search can be used at most {max} times in this request; answer with the results you already have"
            ),
        })
    } else {
        *used += 1;
        client.web_search(&query).await.map_err(|error| {
            error.report("web_search");
            WebSearchError {
                code: "unavailable",
                message: error.user_message(),
            }
        })
    };
    ExecutedSearch {
        call: WebSearchCall {
            tool_use_id: call
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(generate_tool_call_id),
            query,
            outcome,
        },
        input,
    }
}

/// Extends a Kiro payload with one search round, the way the Kiro CLI does:
/// the current user turn moves into history (without its tool specs, which
/// only the current turn carries), followed by the assistant turn with the
/// round's text, `toolUses` and Kiro-signed `reasoningContent`, and a new
/// empty user turn carrying the `toolResults` becomes the current message.
/// A fresh `agentContinuationId` marks the new request.
fn continuation_payload(
    mut payload: Value,
    round_text: &str,
    reasoning: Option<Value>,
    searches: &[ExecutedSearch],
) -> Result<Value> {
    let invalid = || GatewayError::Internal("Kiro payload has no conversationState".into());
    let state = payload
        .get_mut("conversationState")
        .and_then(Value::as_object_mut)
        .ok_or_else(invalid)?;
    let mut current = match state
        .remove("currentMessage")
        .and_then(|mut message| message.get_mut("userInputMessage").map(Value::take))
    {
        Some(Value::Object(message)) => message,
        _ => return Err(invalid()),
    };
    let mut context = match current.remove("userInputMessageContext") {
        Some(Value::Object(context)) => context,
        _ => Map::new(),
    };
    let tools = context.remove("tools");
    if !context.is_empty() {
        current.insert("userInputMessageContext".into(), Value::Object(context));
    }
    // Keep the Kiro CLI's field order (`content`, context, `origin`, ...).
    let current = reorder_user_message(current);

    let mut next_context = Map::new();
    next_context.insert(
        "toolResults".into(),
        searches.iter().map(tool_result).collect(),
    );
    if let Some(tools) = tools {
        next_context.insert("tools".into(), tools);
    }
    let mut next = Map::new();
    next.insert("content".into(), Value::String(String::new()));
    next.insert(
        "userInputMessageContext".into(),
        Value::Object(next_context),
    );
    for key in ["origin", "modelId"] {
        if let Some(value) = current.get(key) {
            next.insert(key.into(), value.clone());
        }
    }

    let tool_uses: Vec<Value> = searches
        .iter()
        .map(|search| {
            json!({"toolUseId": search.call.tool_use_id, "name": WEB_SEARCH_TOOL_NAME, "input": search.input})
        })
        .collect();
    let mut assistant = Map::new();
    assistant.insert("content".into(), Value::String(round_text.to_owned()));
    assistant.insert("toolUses".into(), Value::Array(tool_uses));
    if let Some(reasoning) = reasoning {
        assistant.insert("reasoningContent".into(), reasoning);
    }
    let Value::Array(history) = state
        .entry("history")
        .or_insert_with(|| Value::Array(Vec::new()))
    else {
        return Err(GatewayError::Internal(
            "Kiro payload history is not an array".into(),
        ));
    };
    history.push(json!({"userInputMessage": current}));
    history.push(json!({"assistantResponseMessage": assistant}));
    state.insert("currentMessage".into(), json!({"userInputMessage": next}));
    state.insert(
        "agentContinuationId".into(),
        Value::String(generate_continuation_id()),
    );
    let reordered = reorder_state(std::mem::take(state));
    *state = reordered;
    Ok(payload)
}

/// Restores the Kiro CLI's `conversationState` field order. (`Map::remove`
/// swap-removes under `preserve_order`, which moves the last key into the
/// removed one's place.)
fn reorder_state(state: Map<String, Value>) -> Map<String, Value> {
    reorder(
        state,
        &[
            "currentMessage",
            "chatTriggerType",
            "conversationId",
            "history",
            "agentContinuationId",
            "agentTaskType",
            "rootConversationId",
        ],
    )
}

/// Orders a `userInputMessage`'s fields as the Kiro CLI sends them.
fn reorder_user_message(message: Map<String, Value>) -> Map<String, Value> {
    reorder(
        message,
        &[
            "content",
            "userInputMessageContext",
            "origin",
            "images",
            "modelId",
        ],
    )
}

/// Rebuilds `map` with the keys in `order` first, in that order, followed by
/// any other keys in their existing order.
fn reorder(map: Map<String, Value>, order: &[&str]) -> Map<String, Value> {
    let mut entries: Vec<(String, Value)> = map.into_iter().collect();
    let mut ordered = Map::new();
    for key in order {
        if let Some(index) = entries.iter().position(|(name, _)| name == key) {
            let (name, value) = entries.remove(index);
            ordered.insert(name, value);
        }
    }
    ordered.extend(entries);
    ordered
}

/// Kiro `toolResults` entry for one search: the result document as JSON
/// (as the Kiro CLI sends it), or an error explanation for the model.
fn tool_result(search: &ExecutedSearch) -> Value {
    match &search.call.outcome {
        Ok(response) => json!({
            "toolUseId": search.call.tool_use_id,
            "content": [{"json": response.raw}],
            "status": "success"
        }),
        Err(error) => json!({
            "toolUseId": search.call.tool_use_id,
            "content": [{"text": format!("web_search failed ({}): {}", error.code, error.message)}],
            "status": "error"
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::WebSearchResponse;

    fn payload() -> Value {
        json!({
            "conversationState": {
                "currentMessage": {"userInputMessage": {
                    "content": "latest rust release?",
                    "userInputMessageContext": {
                        "tools": [{"toolSpecification": {"name": "web_search"}}]
                    },
                    "origin": "KIRO_CLI",
                    "modelId": "claude-sonnet-4.5"
                }},
                "conversationId": "c",
                "history": [{"userInputMessage": {"content": "earlier"}}, {"assistantResponseMessage": {"content": "ok"}}],
                "agentContinuationId": "first"
            },
            "profileArn": "arn",
            "agentMode": "default-v2"
        })
    }

    fn search(outcome: std::result::Result<WebSearchResponse, WebSearchError>) -> ExecutedSearch {
        ExecutedSearch {
            call: WebSearchCall {
                tool_use_id: "tooluse_1".into(),
                query: "rust".into(),
                outcome,
            },
            input: json!({"query": "rust"}),
        }
    }

    #[test]
    fn continuation_moves_turn_to_history_and_attaches_results() {
        let response = WebSearchResponse {
            raw: json!({"results": [{"title": "Rust"}]}),
            ..WebSearchResponse::default()
        };
        let reasoning = json!({"reasoningText": {"text": "...", "signature": ".KTR~~sig"}});
        let next = continuation_payload(
            payload(),
            "Searching.",
            Some(reasoning.clone()),
            &[search(Ok(response))],
        )
        .unwrap_or_else(|error| panic!("continuation must build: {error}"));
        let state = &next["conversationState"];
        let keys: Vec<&String> = state
            .as_object()
            .map(|state| state.keys().collect())
            .unwrap_or_default();
        assert_eq!(
            keys,
            [
                "currentMessage",
                "conversationId",
                "history",
                "agentContinuationId"
            ],
            "Kiro CLI field order"
        );
        assert_ne!(state["agentContinuationId"], "first");
        let history = state["history"]
            .as_array()
            .unwrap_or_else(|| panic!("history must be an array"));
        assert_eq!(history.len(), 4);
        let moved = &history[2]["userInputMessage"];
        assert_eq!(moved["content"], "latest rust release?");
        assert!(moved.get("userInputMessageContext").is_none());
        let moved_keys: Vec<&String> = moved
            .as_object()
            .map(|moved| moved.keys().collect())
            .unwrap_or_default();
        assert_eq!(moved_keys, ["content", "origin", "modelId"]);
        let assistant = &history[3]["assistantResponseMessage"];
        assert_eq!(assistant["content"], "Searching.");
        assert_eq!(
            assistant["toolUses"][0],
            json!({"toolUseId": "tooluse_1", "name": "web_search", "input": {"query": "rust"}})
        );
        assert_eq!(assistant["reasoningContent"], reasoning);
        let current = &state["currentMessage"]["userInputMessage"];
        assert_eq!(current["content"], "");
        assert_eq!(current["modelId"], "claude-sonnet-4.5");
        let context = &current["userInputMessageContext"];
        assert_eq!(
            context["tools"][0]["toolSpecification"]["name"],
            "web_search"
        );
        assert!(context.get("envState").is_none());
        assert_eq!(
            context["toolResults"][0],
            json!({"toolUseId": "tooluse_1", "content": [{"json": {"results": [{"title": "Rust"}]}}], "status": "success"})
        );
        assert_eq!(next["profileArn"], "arn");
        assert_eq!(next["agentMode"], "default-v2");
    }

    #[test]
    fn round_reasoning_keeps_only_kiro_signed_or_redacted_reasoning() {
        assert!(RoundReasoning::default().into_value().is_none());

        let mut hidden = RoundReasoning::default();
        hidden.apply(&KiroEvent::thinking(None, Some(".KTR~~sig".into())));
        assert_eq!(
            hidden.into_value(),
            Some(json!({"reasoningText": {"text": "...", "signature": ".KTR~~sig"}}))
        );

        let mut summarized = RoundReasoning::default();
        summarized.apply(&KiroEvent::thinking(Some("Let me ".into()), None));
        summarized.apply(&KiroEvent::thinking(
            Some("search.".into()),
            Some("Ep0C".into()),
        ));
        assert_eq!(
            summarized.into_value(),
            Some(json!({"reasoningText": {"text": "Let me search.", "signature": "Ep0C"}}))
        );

        let mut unsigned = RoundReasoning::default();
        unsigned.apply(&KiroEvent::thinking(Some("no signature".into()), None));
        assert!(unsigned.into_value().is_none());

        let mut redacted = RoundReasoning::default();
        redacted.apply(&KiroEvent::redacted_thinking("LktUUn4="));
        assert_eq!(
            redacted.into_value(),
            Some(json!({"redactedContent": "LktUUn4="}))
        );
    }

    #[test]
    fn failed_search_becomes_error_tool_result() {
        let failed = search(Err(WebSearchError {
            code: "unavailable",
            message: "boom".into(),
        }));
        let result = tool_result(&failed);
        assert_eq!(result["status"], "error");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("unavailable") && text.contains("boom"))
        );
    }

    #[tokio::test]
    async fn disabled_loop_passes_tool_calls_through() {
        let config = Arc::new(Config::default());
        let auth = Arc::new(
            AuthManager::new((*config).clone())
                .unwrap_or_else(|error| panic!("auth manager must construct: {error}")),
        );
        let body = br#"{"content":"hi"}{"name":"web_search","toolUseId":"t1","input":{"query":"x"},"stop":true}"#;
        let first: ByteStream = stream::iter(vec![Ok(Bytes::from_static(body))]).boxed();
        let search = SearchLoop {
            auth,
            config,
            max_searches: None,
        };
        let (result, searches) = collect(search.events(first, payload()))
            .await
            .unwrap_or_else(|error| panic!("loop must collect: {error}"));
        assert_eq!(result.content, "hi");
        assert_eq!(result.tool_calls.len(), 1);
        assert!(searches.is_empty());
    }
}

#[cfg(test)]
mod preflight_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    #[tokio::test]
    async fn first_byte_timeout_retries_before_returning_a_stream() {
        let mut sources = VecDeque::from([
            stream::pending::<std::result::Result<bytes::Bytes, reqwest::Error>>().boxed(),
            stream::once(async {
                Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from_static(b"first"))
            })
            .boxed(),
        ]);
        let mut requests = 0_u8;
        let mut source = preflight_first_byte(2, Duration::from_millis(1), || {
            requests = requests.saturating_add(1);
            let next = sources.pop_front();
            async move { next.ok_or_else(|| GatewayError::Internal("missing test stream".into())) }
        })
        .await
        .unwrap_or_else(|error| panic!("preflight should retry before emitting: {error}"));
        assert_eq!(requests, 2);
        let first = source
            .next()
            .await
            .unwrap_or_else(|| panic!("replayed first byte must be available"))
            .unwrap_or_else(|error| panic!("test stream must be successful: {error}"));
        assert_eq!(first, bytes::Bytes::from_static(b"first"));
    }

    // Serves one response that sends headers and then drops the connection
    // before any body chunk, so reading the first chunk yields a real
    // `reqwest::Error`.
    async fn truncated_body_stream()
    -> futures_util::stream::BoxStream<'static, std::result::Result<bytes::Bytes, reqwest::Error>>
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("bind test listener: {error}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("listener address: {error}"));
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0_u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .await;
            }
        });
        reqwest::get(format!("http://{addr}/"))
            .await
            .unwrap_or_else(|error| panic!("test request: {error}"))
            .bytes_stream()
            .boxed()
    }

    #[tokio::test]
    async fn first_chunk_network_error_retries_before_returning_a_stream() {
        let mut sources = VecDeque::from([
            truncated_body_stream().await,
            stream::once(async {
                Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from_static(b"first"))
            })
            .boxed(),
        ]);
        let mut requests = 0_u8;
        let mut source = preflight_first_byte(2, Duration::from_secs(5), || {
            requests = requests.saturating_add(1);
            let next = sources.pop_front();
            async move { next.ok_or_else(|| GatewayError::Internal("missing test stream".into())) }
        })
        .await
        .unwrap_or_else(|error| panic!("preflight should retry a dropped first chunk: {error}"));
        assert_eq!(requests, 2);
        let first = source
            .next()
            .await
            .unwrap_or_else(|| panic!("replayed first byte must be available"))
            .unwrap_or_else(|error| panic!("test stream must be successful: {error}"));
        assert_eq!(first, bytes::Bytes::from_static(b"first"));
    }

    #[tokio::test]
    async fn first_chunk_network_error_surfaces_after_last_attempt() {
        let mut requests = 0_u8;
        let result = preflight_first_byte(2, Duration::from_secs(5), || {
            requests = requests.saturating_add(1);
            async { Ok(truncated_body_stream().await) }
        })
        .await;
        assert_eq!(requests, 2);
        assert!(matches!(result, Err(GatewayError::Network(_))));
    }
}
