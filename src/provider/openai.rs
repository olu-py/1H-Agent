use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};
#[cfg(test)]
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use super::{
    ConversationItem, ModelEvent, ModelRequest, ProviderError, Role, ThinkingMode, ToolCall,
    ToolDefinition, Usage,
};
use crate::config::{ProviderKind, ThinkingLevel, ThinkingProfileKind};

#[derive(Clone)]
pub struct OpenAiClient {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
    #[cfg(test)]
    scripted_events: Option<std::sync::Arc<Mutex<std::collections::VecDeque<Vec<ModelEvent>>>>>,
}

impl OpenAiClient {
    pub fn new(base_url: String, api_key: String) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(10 * 60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            client,
            #[cfg(test)]
            scripted_events: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn scripted(responses: Vec<Vec<ModelEvent>>) -> Result<Self, ProviderError> {
        let mut client = Self::new("http://127.0.0.1".into(), "test".into())?;
        client.scripted_events = Some(std::sync::Arc::new(Mutex::new(responses.into())));
        Ok(client)
    }

    pub async fn stream(
        &self,
        request: ModelRequest,
        events: mpsc::Sender<ModelEvent>,
    ) -> Result<(), ProviderError> {
        #[cfg(test)]
        if let Some(scripted) = &self.scripted_events {
            let response =
                scripted.lock().await.pop_front().ok_or_else(|| {
                    ProviderError::Protocol("scripted provider exhausted".to_owned())
                })?;
            for event in response {
                events
                    .send(event)
                    .await
                    .map_err(|_| ProviderError::ReceiverClosed)?;
            }
            return Ok(());
        }
        let (path, body) = match request.kind {
            ProviderKind::ChatCompletions => ("chat/completions", chat_body(&request)),
            ProviderKind::Responses => ("responses", responses_body(&request)),
        };
        let response = self
            .client
            .post(format!("{}/{path}", self.base_url))
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(ACCEPT, "text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await?;
            let message =
                String::from_utf8_lossy(&bytes[..bytes.len().min(16 * 1024)]).into_owned();
            return Err(ProviderError::Status {
                status: status.as_u16(),
                message,
            });
        }

        let mut decoder = SseDecoder::default();
        let mut stream = response.bytes_stream();
        let mut saw_done = false;
        while let Some(chunk) = stream.next().await {
            for event in decoder.push(&chunk?) {
                if event.data.trim() == "[DONE]" {
                    saw_done = true;
                    continue;
                }
                let value: Value = serde_json::from_str(&event.data)
                    .map_err(|error| ProviderError::Protocol(error.to_string()))?;
                let parsed = match request.kind {
                    ProviderKind::ChatCompletions => parse_chat_event(&value),
                    ProviderKind::Responses => {
                        parse_responses_event_for_mode(&value, request.thinking_mode)
                    }
                }?;
                for model_event in parsed {
                    if matches!(model_event, ModelEvent::Done) {
                        saw_done = true;
                    }
                    events
                        .send(model_event)
                        .await
                        .map_err(|_| ProviderError::ReceiverClosed)?;
                }
            }
        }
        if !saw_done {
            events
                .send(ModelEvent::Done)
                .await
                .map_err(|_| ProviderError::ReceiverClosed)?;
        } else if request.kind == ProviderKind::ChatCompletions {
            events
                .send(ModelEvent::Done)
                .await
                .map_err(|_| ProviderError::ReceiverClosed)?;
        }
        Ok(())
    }
}

fn chat_body(request: &ModelRequest) -> Value {
    let messages: Vec<Value> = request.items.iter().filter_map(chat_item).collect();
    let tools: Vec<Value> = request.tools.iter().map(chat_tool).collect();
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    apply_chat_thinking(&mut body, request);
    body
}

fn apply_chat_thinking(body: &mut Value, request: &ModelRequest) {
    match request.thinking_profile_kind {
        ThinkingProfileKind::Qwen38 => {
            if let Some(effort) = reasoning_effort(request.thinking_level) {
                body["reasoning_effort"] = Value::String(effort.into());
            }
        }
        ThinkingProfileKind::Qwen37 => {
            let enabled = request.thinking_level == ThinkingLevel::Enabled;
            body["enable_thinking"] = Value::Bool(enabled);
            if enabled && let Some(budget) = request.thinking_budget_tokens {
                body["thinking_budget"] = Value::Number(budget.into());
            }
        }
        ThinkingProfileKind::DeepSeekPro | ThinkingProfileKind::DeepSeekFlash => {
            let enabled = request.thinking_level != ThinkingLevel::None;
            body["thinking"] = json!({"type": if enabled { "enabled" } else { "disabled" }});
            if let Some(effort) = deepseek_effort(request) {
                body["reasoning_effort"] = Value::String(effort.into());
            }
        }
        ThinkingProfileKind::Volcano => {
            body["thinking"] = json!({"type": "enabled"});
        }
        ThinkingProfileKind::OpenAi | ThinkingProfileKind::Compatible => {
            if let Some(effort) = reasoning_effort(request.thinking_level) {
                body["reasoning_effort"] = Value::String(effort.into());
            }
        }
    }
}

fn chat_item(item: &ConversationItem) -> Option<Value> {
    match item {
        ConversationItem::Message { role, content } => Some(json!({
            "role": role_name(*role),
            "content": content,
        })),
        ConversationItem::ThinkingSummary { .. } => None,
        ConversationItem::Context { label, content } => Some(json!({
            "role": "user",
            "content": format!("[Context: {label}]\n{content}"),
        })),
        ConversationItem::ProviderItem { .. } => None,
        ConversationItem::AssistantToolCalls { calls } => Some(json!({
            "role": "assistant",
            "tool_calls": calls.iter().map(|call| json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": call.arguments.to_string(),
                }
            })).collect::<Vec<_>>()
        })),
        ConversationItem::ToolOutput { call_id, output } => Some(json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": output,
        })),
    }
}

fn responses_body(request: &ModelRequest) -> Value {
    let mut instructions = None;
    let input: Vec<Value> = request
        .items
        .iter()
        .filter(|item| {
            if instructions.is_none()
                && let ConversationItem::Message {
                    role: Role::System,
                    content,
                } = item
            {
                instructions = Some(content.clone());
                false
            } else {
                true
            }
        })
        .flat_map(responses_item)
        .collect();
    let mut tools: Vec<Value> = request
        .tools
        .iter()
        .filter(|tool| !(request.native_web_search && tool.name == "web_search"))
        .map(responses_tool)
        .collect();
    if request.native_web_search {
        tools.push(json!({"type": "web_search"}));
    }
    let mut body = json!({
        "model": request.model,
        "input": input,
        "stream": true,
    });
    apply_responses_thinking(&mut body, request);
    if let Some(instructions) = instructions {
        body["instructions"] = Value::String(instructions);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(id) = &request.previous_response_id {
        body["previous_response_id"] = Value::String(id.clone());
    }
    body
}

fn apply_responses_thinking(body: &mut Value, request: &ModelRequest) {
    match request.thinking_profile_kind {
        ThinkingProfileKind::DeepSeekPro | ThinkingProfileKind::DeepSeekFlash => {
            let enabled = request.thinking_level != ThinkingLevel::None;
            body["thinking"] = json!({"type": if enabled { "enabled" } else { "disabled" }});
            if let Some(effort) = deepseek_effort(request) {
                body["reasoning"] = json!({"effort": effort});
            }
        }
        ThinkingProfileKind::OpenAi => {
            let mut reasoning = serde_json::Map::new();
            if request.thinking_level != ThinkingLevel::None {
                reasoning.insert("summary".into(), Value::String("auto".into()));
            }
            if let Some(effort) = reasoning_effort(request.thinking_level) {
                reasoning.insert("effort".into(), Value::String(effort.into()));
            }
            if !reasoning.is_empty() {
                body["reasoning"] = Value::Object(reasoning);
            }
        }
        ThinkingProfileKind::Compatible => {
            if let Some(effort) = reasoning_effort(request.thinking_level) {
                body["reasoning"] = json!({"effort": effort});
            }
        }
        ThinkingProfileKind::Qwen38 | ThinkingProfileKind::Qwen37 => {
            if let Some(effort) = reasoning_effort(request.thinking_level) {
                body["reasoning"] = json!({"effort": effort});
            }
        }
        ThinkingProfileKind::Volcano => {}
    }
}

fn reasoning_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Auto | ThinkingLevel::Enabled => None,
        ThinkingLevel::None => Some("none"),
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::XHigh => Some("xhigh"),
        ThinkingLevel::Max => Some("max"),
    }
}

fn deepseek_effort(request: &ModelRequest) -> Option<&'static str> {
    match request.thinking_level {
        ThinkingLevel::None => Some("none"),
        ThinkingLevel::XHigh
            if request.thinking_profile_kind == ThinkingProfileKind::DeepSeekFlash =>
        {
            Some("high")
        }
        level => reasoning_effort(level),
    }
}

fn responses_item(item: &ConversationItem) -> Vec<Value> {
    match item {
        ConversationItem::Message { role, content } => vec![json!({
            "role": role_name(*role),
            "content": content,
        })],
        ConversationItem::ThinkingSummary { .. } => Vec::new(),
        ConversationItem::Context { label, content } => vec![json!({
            "role": "user",
            "content": format!("[Context: {label}]\n{content}"),
        })],
        ConversationItem::ProviderItem { item } => vec![item.clone()],
        ConversationItem::AssistantToolCalls { calls } => calls
            .iter()
            .map(|call| {
                json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments.to_string(),
                })
            })
            .collect(),
        ConversationItem::ToolOutput { call_id, output } => vec![json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        })],
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn chat_tool(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
    })
}

fn responses_tool(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

pub(crate) fn parse_chat_event(value: &Value) -> Result<Vec<ModelEvent>, ProviderError> {
    let mut events = Vec::new();
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        events.push(ModelEvent::Usage(parse_usage(usage)));
    }
    for choice in value
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if let Some(reasoning) = recognized_reasoning_delta(delta) {
            events.push(ModelEvent::ReasoningDelta(reasoning.to_owned()));
        }
        if let Some(content) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
        {
            events.push(ModelEvent::TextDelta(content.to_owned()));
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let function = call.get("function").unwrap_or(&Value::Null);
            events.push(ModelEvent::ToolCallDelta {
                slot: index.to_string(),
                id: call.get("id").and_then(Value::as_str).map(str::to_owned),
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                arguments_delta: function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
    }
    if let Some(reasoning) = value.get("delta").and_then(recognized_reasoning_delta) {
        events.push(ModelEvent::ReasoningDelta(reasoning.to_owned()));
    }
    append_native_search_events(value, &mut events);
    Ok(events)
}

#[cfg(test)]
pub(crate) fn parse_responses_event(value: &Value) -> Result<Vec<ModelEvent>, ProviderError> {
    parse_responses_event_for_mode(value, ThinkingMode::OpenAiResponsesSummary)
}

pub(crate) fn parse_responses_event_for_mode(
    value: &Value,
    thinking_mode: ThinkingMode,
) -> Result<Vec<ModelEvent>, ProviderError> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut events = Vec::new();
    append_native_search_events(value, &mut events);
    match event_type {
        "response.created" | "response.in_progress" => {
            if let Some(id) = value.pointer("/response/id").and_then(Value::as_str) {
                events.push(ModelEvent::ResponseId(id.to_owned()));
            }
        }
        "response.output_text.delta" => {
            if let Some(delta) = value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|delta| !delta.is_empty())
            {
                events.push(ModelEvent::TextDelta(delta.to_owned()));
            }
        }
        "response.reasoning_summary_text.delta"
            if matches!(thinking_mode, ThinkingMode::OpenAiResponsesSummary) =>
        {
            if let Some(delta) = value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|delta| safe_reasoning_text(delta))
            {
                events.push(ModelEvent::ReasoningDelta(delta.to_owned()));
            }
        }
        "response.reasoning_text.delta"
        | "response.reasoning_content.delta"
        | "response.reasoning.delta"
            if matches!(
                thinking_mode,
                ThinkingMode::DeepSeekResponses | ThinkingMode::QwenResponses
            ) =>
        {
            if let Some(delta) = value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|delta| safe_reasoning_text(delta))
            {
                events.push(ModelEvent::ReasoningDelta(delta.to_owned()));
            }
        }
        // This event carries the complete text accumulated from all preceding
        // deltas. Re-emitting it as a delta duplicates and fragments summaries.
        "response.reasoning_text.done"
            if matches!(
                thinking_mode,
                ThinkingMode::DeepSeekResponses | ThinkingMode::QwenResponses
            ) => {}
        "response.output_item.added" => {
            let item = value.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                events.push(ModelEvent::ToolCallDelta {
                    slot: response_slot(value, item),
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    name: item.get("name").and_then(Value::as_str).map(str::to_owned),
                    arguments_delta: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
        }
        "response.function_call_arguments.delta" => {
            events.push(ModelEvent::ToolCallDelta {
                slot: response_slot(value, &Value::Null),
                id: None,
                name: None,
                arguments_delta: value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
        "response.output_item.done" => {
            let item = value.get("item").unwrap_or(&Value::Null);
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    events.push(ModelEvent::ToolCallComplete(ToolCall {
                        id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        arguments: serde_json::from_str(arguments).map_err(|error| {
                            ProviderError::Protocol(format!("invalid tool arguments: {error}"))
                        })?,
                    }));
                }
                Some("reasoning") => {
                    append_reasoning_summary(item, &mut events);
                    events.push(ModelEvent::ProviderItem(item.clone()));
                }
                Some("web_search_call") => {
                    events.push(ModelEvent::ProviderItem(item.clone()));
                }
                _ => {}
            }
        }
        "response.completed" => {
            if let Some(response) = value.get("response") {
                if let Some(id) = response.get("id").and_then(Value::as_str) {
                    events.push(ModelEvent::ResponseId(id.to_owned()));
                }
                if let Some(usage) = response.get("usage") {
                    events.push(ModelEvent::Usage(parse_usage(usage)));
                }
            }
            events.push(ModelEvent::Done);
        }
        "response.incomplete" => {
            let reason = value
                .pointer("/response/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("output was truncated");
            return Err(ProviderError::Protocol(format!(
                "provider response incomplete: {reason}"
            )));
        }
        "response.failed" | "error" => {
            let message = value
                .pointer("/response/error/message")
                .or_else(|| value.pointer("/error/message"))
                .and_then(Value::as_str)
                .unwrap_or("provider reported an unknown error");
            return Err(ProviderError::Protocol(message.to_owned()));
        }
        _ => {}
    }
    Ok(events)
}

fn recognized_reasoning_delta(delta: &Value) -> Option<&str> {
    ["reasoning_content", "thinking", "reasoning"]
        .into_iter()
        .find_map(|field| delta.get(field).and_then(Value::as_str))
        .filter(|value| safe_reasoning_text(value))
}

fn append_reasoning_summary(item: &Value, events: &mut Vec<ModelEvent>) {
    for text in item
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|summary| summary.get("text").and_then(Value::as_str))
        .filter(|text| safe_reasoning_text(text))
    {
        events.push(ModelEvent::ReasoningDelta(text.to_owned()));
    }
}

fn safe_reasoning_text(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return false;
    }
    let looks_base64 = trimmed.len() >= 32
        && trimmed.len().rem_euclid(4) == 0
        && trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='));
    !looks_base64
}

// DeepSeek Responses follows the OpenAI web-search event and citation shapes.
// Keep support for compatible gateways that attach query/result fields directly.
fn append_native_search_events(value: &Value, events: &mut Vec<ModelEvent>) {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let item = value.get("item").unwrap_or(&Value::Null);
    let is_search_item = item.get("type").and_then(Value::as_str) == Some("web_search_call");
    let is_search_event = event_type.contains("web_search") || event_type.contains("search");
    if let Some(query) = value
        .get("query")
        .or_else(|| value.pointer("/search/query"))
        .or_else(|| item.pointer("/action/query"))
        .and_then(Value::as_str)
    {
        events.push(ModelEvent::WebSearchStarted {
            query: bounded_text(query, 512),
        });
    } else if is_search_event && !event_type.contains("completed") {
        events.push(ModelEvent::WebSearchStarted {
            query: "DeepSeek 服务端网络搜索".into(),
        });
    }
    if let Some(result) = value.get("result").or_else(|| value.get("source")) {
        let title = result
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Search result");
        let url = result
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let snippet = result
            .get("snippet")
            .or_else(|| result.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !url.is_empty() {
            events.push(ModelEvent::WebSearchResult {
                title: bounded_text(title, 256),
                url: bounded_text(url, 2048),
                snippet: bounded_text(snippet, 4096),
            });
        }
    }
    append_url_citations(item, events);
    if event_type.contains("completed") || (is_search_item && event_type.ends_with(".done")) {
        events.push(ModelEvent::WebSearchCompleted {
            count: value.get("count").and_then(Value::as_u64).unwrap_or(0) as usize,
        });
    }
}

fn append_url_citations(item: &Value, events: &mut Vec<ModelEvent>) {
    for content in item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for annotation in content
            .get("annotations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(10)
        {
            if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
                continue;
            }
            let url = annotation
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !url.is_empty() {
                events.push(ModelEvent::WebSearchResult {
                    title: bounded_text(
                        annotation
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("搜索来源"),
                        256,
                    ),
                    url: bounded_text(url, 2048),
                    snippet: String::new(),
                });
            }
        }
    }
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub(3);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

fn response_slot(event: &Value, item: &Value) -> String {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .map(|value| value.to_string())
        .or_else(|| item.get("id").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "0".into())
}

fn parse_usage(value: &Value) -> Usage {
    let input_tokens = value
        .get("input_tokens")
        .or_else(|| value.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = value
        .get("output_tokens")
        .or_else(|| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens,
        output_tokens,
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(input_tokens + output_tokens),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &Bytes) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((position, delimiter_len)) = find_event_boundary(&self.buffer) {
            let frame = self.buffer.drain(..position).collect::<Vec<_>>();
            self.buffer.drain(..delimiter_len);
            if let Some(event) = parse_sse_frame(&frame) {
                events.push(event);
            }
        }
        events
    }
}

fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| (position, 2))
        })
}

fn parse_sse_frame(frame: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(frame);
    let mut event = None;
    let mut data = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    (!data.is_empty()).then(|| SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fragmented_sse() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(&Bytes::from("event: x\ndata: {\"a\":"))
                .is_empty()
        );
        let events = decoder.push(&Bytes::from("1}\n\n"));
        assert_eq!(events[0].event.as_deref(), Some("x"));
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn parses_chat_text_and_tool_delta() {
        let value = json!({"choices":[{"delta":{"content":"hi","tool_calls":[{
            "index":0,"id":"call_1","function":{"name":"file_read","arguments":"{\"path\":"}
        }]}}]});
        let events = parse_chat_event(&value).unwrap();
        assert!(matches!(&events[0], ModelEvent::TextDelta(text) if text == "hi"));
        assert!(
            matches!(&events[1], ModelEvent::ToolCallDelta { name: Some(name), .. } if name == "file_read")
        );
    }

    #[test]
    fn qwen_reasoning_chunk_does_not_emit_empty_text_delta() {
        let events = parse_chat_event(&json!({
            "model":"qwen3.8-max",
            "choices":[{"delta":{
                "reasoning_content":"同一句话的增量",
                "content":""
            }}]
        }))
        .unwrap();
        assert_eq!(
            events,
            vec![ModelEvent::ReasoningDelta("同一句话的增量".into())]
        );
    }

    #[test]
    fn parses_responses_text() {
        let events = parse_responses_event(&json!({
            "type":"response.output_text.delta", "delta":"hello"
        }))
        .unwrap();
        assert_eq!(events, vec![ModelEvent::TextDelta("hello".into())]);
    }

    #[test]
    fn parses_bounded_compatible_search_events() {
        let events = parse_responses_event(&json!({
            "type": "response.web_search.result",
            "query": "rust async",
            "result": {
                "title": "Tokio",
                "url": "https://tokio.rs",
                "snippet": "runtime"
            }
        }))
        .unwrap();
        assert!(matches!(
            events.first(),
            Some(ModelEvent::WebSearchStarted { query }) if query == "rust async"
        ));
        assert!(matches!(
            events.get(1),
            Some(ModelEvent::WebSearchResult { url, .. }) if url == "https://tokio.rs"
        ));
    }

    #[test]
    fn parses_deepseek_web_search_status_and_citation() {
        let status = parse_responses_event(&json!({
            "type": "response.web_search_call.searching",
            "item_id": "ws_1"
        }))
        .unwrap();
        assert!(matches!(
            status.first(),
            Some(ModelEvent::WebSearchStarted { query }) if query.contains("DeepSeek")
        ));

        let done = parse_responses_event(&json!({
            "type": "response.output_item.done",
            "item": {
                "id": "msg_1",
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": "source",
                    "annotations": [{
                        "type": "url_citation",
                        "title": "DeepSeek Docs",
                        "url": "https://api-docs.deepseek.com/"
                    }]
                }]
            }
        }))
        .unwrap();
        assert!(matches!(
            done.first(),
            Some(ModelEvent::WebSearchResult { url, .. }) if url.contains("deepseek.com")
        ));
    }

    #[test]
    fn parses_reasoning_summary_and_rejects_incomplete_response() {
        let events = parse_responses_event(&json!({
            "type": "response.output_item.done",
            "item": {
                "id":"rs_1",
                "type":"reasoning",
                "encrypted_content":"never show",
                "summary":[{"type":"summary_text", "text":"checked constraints"}]
            }
        }))
        .unwrap();
        assert!(matches!(
            events.first(),
            Some(ModelEvent::ReasoningDelta(text)) if text == "checked constraints"
        ));
        assert!(matches!(
            events.get(1),
            Some(ModelEvent::ProviderItem(item)) if item["id"] == "rs_1"
        ));

        let error = parse_responses_event(&json!({
            "type": "response.incomplete",
            "response": {"incomplete_details":{"reason":"max_output_tokens"}}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("max_output_tokens"));
    }

    #[test]
    fn parses_openai_reasoning_summary_delta() {
        let events = parse_responses_event(&json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":"rs_123",
            "output_index":0,
            "summary_index":0,
            "delta":"working",
            "sequence_number":7
        }))
        .unwrap();
        assert_eq!(events, vec![ModelEvent::ReasoningDelta("working".into())]);
    }

    #[test]
    fn parses_official_deepseek_responses_reasoning_text_events() {
        let delta = parse_responses_event_for_mode(
            &json!({
                "type":"response.reasoning_text.delta",
                "response_id":"resp_123",
                "item_id":"rs_123",
                "output_index":0,
                "content_index":0,
                "delta":"检查项目结构"
            }),
            ThinkingMode::DeepSeekResponses,
        )
        .unwrap();
        assert_eq!(
            delta,
            vec![ModelEvent::ReasoningDelta("检查项目结构".into())]
        );

        let done = parse_responses_event_for_mode(
            &json!({
                "type":"response.reasoning_text.done",
                "response_id":"resp_123",
                "item_id":"rs_123",
                "text":"检查项目结构"
            }),
            ThinkingMode::DeepSeekResponses,
        )
        .unwrap();
        assert!(done.is_empty());
    }

    #[test]
    fn parses_qwen_responses_reasoning_deltas_without_replaying_done_text() {
        let delta = parse_responses_event_for_mode(
            &json!({
                "type":"response.reasoning_text.delta",
                "delta":"正在检查",
                "item_id":"msg_1",
                "output_index":0
            }),
            ThinkingMode::QwenResponses,
        )
        .unwrap();
        assert_eq!(delta, vec![ModelEvent::ReasoningDelta("正在检查".into())]);

        let done = parse_responses_event_for_mode(
            &json!({
                "type":"response.reasoning_text.done",
                "text":"正在检查项目结构",
                "item_id":"msg_1",
                "output_index":0
            }),
            ThinkingMode::QwenResponses,
        )
        .unwrap();
        assert!(done.is_empty());
    }

    fn assert_chat_reasoning(model: &str, field: &str) {
        let events = parse_chat_event(&json!({
            "id":"chatcmpl-fixture",
            "object":"chat.completion.chunk",
            "created":1770000000,
            "model":model,
            "choices":[{
                "index":0,
                "delta":{"role":"assistant",(field):"reasoning chunk","content":null},
                "finish_reason":null
            }]
        }))
        .unwrap();
        assert_eq!(
            events,
            vec![ModelEvent::ReasoningDelta("reasoning chunk".into())]
        );
    }

    #[test]
    fn parses_deepseek_reasoning_content() {
        assert_chat_reasoning("deepseek-reasoner", "reasoning_content");
    }

    #[test]
    fn parses_qwen_reasoning_content() {
        assert_chat_reasoning("qwen-plus", "reasoning_content");
    }

    #[test]
    fn parses_volcano_reasoning_content() {
        assert_chat_reasoning("doubao-seed-2-1-pro", "reasoning_content");
    }

    #[test]
    fn parses_only_recognized_custom_reasoning_fields() {
        assert_chat_reasoning("custom", "thinking");
        assert_chat_reasoning("custom", "reasoning");
        let direct = parse_chat_event(&json!({
            "delta":{"thinking":"direct"}
        }))
        .unwrap();
        assert_eq!(direct, vec![ModelEvent::ReasoningDelta("direct".into())]);

        for value in [
            json!({"choices":[{"delta":{"encrypted_content":"secret"}}]}),
            json!({"choices":[{"delta":{"unknown":{"reasoning_content":"hidden"}}}]}),
            json!({"choices":[{"delta":{"blob":"SGVsbG8="}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"QUJDREVGR0hJSktMTU5PUFFSU1RVVldY"}}]}),
        ] {
            assert!(parse_chat_event(&value).unwrap().is_empty());
        }
    }

    #[test]
    fn deepseek_responses_body_uses_native_search_and_instructions() {
        let request = ModelRequest {
            kind: ProviderKind::Responses,
            model: "deepseek-v4-flash".into(),
            items: vec![
                ConversationItem::Message {
                    role: Role::System,
                    content: "stable system prefix".into(),
                },
                ConversationItem::Message {
                    role: Role::User,
                    content: "latest news".into(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "web_search".into(),
                description: "local fallback".into(),
                parameters: json!({"type":"object"}),
            }],
            previous_response_id: None,
            native_web_search: true,
            thinking_mode: ThinkingMode::Disabled,
            thinking_level: ThinkingLevel::High,
            thinking_budget_tokens: None,
            thinking_profile_kind: ThinkingProfileKind::DeepSeekFlash,
        };
        let body = responses_body(&request);
        assert_eq!(body["instructions"], "stable system prefix");
        assert_eq!(body["tools"], json!([{"type":"web_search"}]));
        assert_eq!(
            body.pointer("/input/0/content"),
            Some(&json!("latest news"))
        );
    }

    #[test]
    fn provider_body_uses_only_declared_function_tools() {
        let request = ModelRequest {
            kind: ProviderKind::ChatCompletions,
            model: "deepseek-v4".into(),
            items: vec![ConversationItem::Message {
                role: Role::User,
                content: "search".into(),
            }],
            tools: Vec::new(),
            previous_response_id: None,
            native_web_search: false,
            thinking_mode: ThinkingMode::Disabled,
            thinking_level: ThinkingLevel::Auto,
            thinking_budget_tokens: None,
            thinking_profile_kind: ThinkingProfileKind::Compatible,
        };
        let body = chat_body(&request);
        assert!(body.get("tools").is_none());
        assert_eq!(body.pointer("/messages/0/content"), Some(&json!("search")));
    }

    #[test]
    fn qwen37_thinking_switch_and_budget_are_serialized() {
        let mut request = ModelRequest {
            kind: ProviderKind::ChatCompletions,
            model: "qwen3-max".into(),
            items: Vec::new(),
            tools: Vec::new(),
            previous_response_id: None,
            native_web_search: false,
            thinking_mode: ThinkingMode::QwenChat,
            thinking_level: ThinkingLevel::Enabled,
            thinking_budget_tokens: Some(4096),
            thinking_profile_kind: ThinkingProfileKind::Qwen37,
        };
        assert_eq!(chat_body(&request)["enable_thinking"], true);
        assert_eq!(chat_body(&request)["thinking_budget"], 4096);
        for budget in [1024, 4096, 8192, 16384, 32768] {
            request.thinking_budget_tokens = Some(budget);
            assert_eq!(chat_body(&request)["thinking_budget"], budget);
        }
        request.thinking_level = ThinkingLevel::None;
        let disabled = chat_body(&request);
        assert_eq!(disabled["enable_thinking"], false);
        assert!(disabled.get("thinking_budget").is_none());
    }

    fn empty_request(kind: ProviderKind, thinking_mode: ThinkingMode) -> ModelRequest {
        ModelRequest {
            kind,
            model: "fixture-model".into(),
            items: Vec::new(),
            tools: Vec::new(),
            previous_response_id: None,
            native_web_search: false,
            thinking_mode,
            thinking_level: ThinkingLevel::Auto,
            thinking_budget_tokens: None,
            thinking_profile_kind: ThinkingProfileKind::Compatible,
        }
    }

    #[test]
    fn provider_specific_thinking_request_fields_are_isolated() {
        let mut openai_request = empty_request(
            ProviderKind::Responses,
            ThinkingMode::OpenAiResponsesSummary,
        );
        openai_request.thinking_profile_kind = ThinkingProfileKind::OpenAi;
        let openai = responses_body(&openai_request);
        assert_eq!(openai["reasoning"], json!({"summary":"auto"}));
        openai_request.thinking_level = ThinkingLevel::High;
        assert_eq!(
            responses_body(&openai_request)["reasoning"],
            json!({"summary":"auto", "effort":"high"})
        );

        let mut deepseek_request =
            empty_request(ProviderKind::Responses, ThinkingMode::DeepSeekResponses);
        deepseek_request.thinking_profile_kind = ThinkingProfileKind::DeepSeekFlash;
        deepseek_request.thinking_level = ThinkingLevel::XHigh;
        let deepseek_responses = responses_body(&deepseek_request);
        assert_eq!(deepseek_responses["reasoning"], json!({"effort":"high"}));
        assert_eq!(deepseek_responses["thinking"], json!({"type":"enabled"}));
        deepseek_request.thinking_level = ThinkingLevel::Max;
        let flash_max = responses_body(&deepseek_request);
        assert_eq!(flash_max["reasoning"], json!({"effort":"max"}));
        assert_eq!(flash_max["thinking"], json!({"type":"enabled"}));
        deepseek_request.kind = ProviderKind::ChatCompletions;
        let flash_max_chat = chat_body(&deepseek_request);
        assert_eq!(flash_max_chat["reasoning_effort"], "max");
        assert_eq!(flash_max_chat["thinking"], json!({"type":"enabled"}));

        let mut volcano = empty_request(ProviderKind::ChatCompletions, ThinkingMode::VolcanoChat);
        volcano.thinking_profile_kind = ThinkingProfileKind::Volcano;
        volcano.thinking_level = ThinkingLevel::High;
        assert_eq!(chat_body(&volcano)["thinking"], json!({"type":"enabled"}));

        let mut qwen38 = empty_request(ProviderKind::ChatCompletions, ThinkingMode::QwenChat);
        qwen38.thinking_profile_kind = ThinkingProfileKind::Qwen38;
        qwen38.thinking_level = ThinkingLevel::XHigh;
        qwen38.thinking_budget_tokens = Some(8192);
        let qwen38_body = chat_body(&qwen38);
        assert_eq!(qwen38_body["reasoning_effort"], "xhigh");
        assert!(qwen38_body.get("enable_thinking").is_none());
        assert!(qwen38_body.get("thinking_budget").is_none());

        qwen38.kind = ProviderKind::Responses;
        qwen38.thinking_mode = ThinkingMode::QwenResponses;
        let qwen38_responses = responses_body(&qwen38);
        assert_eq!(qwen38_responses["reasoning"], json!({"effort":"xhigh"}));

        let mut qwen37 = empty_request(ProviderKind::Responses, ThinkingMode::QwenResponses);
        qwen37.thinking_profile_kind = ThinkingProfileKind::Qwen37;
        qwen37.thinking_level = ThinkingLevel::None;
        assert_eq!(
            responses_body(&qwen37)["reasoning"],
            json!({"effort":"none"})
        );
        qwen37.thinking_level = ThinkingLevel::Enabled;
        assert!(responses_body(&qwen37).get("reasoning").is_none());

        let compatible = chat_body(&empty_request(
            ProviderKind::ChatCompletions,
            ThinkingMode::CompatibleAuto,
        ));
        assert!(compatible.get("thinking").is_none());
        assert!(compatible.get("enable_thinking").is_none());
    }

    #[test]
    fn reasoning_provider_item_is_returned_in_follow_up_input() {
        let item = json!({
            "id":"rs_123",
            "type":"reasoning",
            "summary":[{"type":"summary_text","text":"summary"}]
        });
        let mut request = empty_request(
            ProviderKind::Responses,
            ThinkingMode::OpenAiResponsesSummary,
        );
        request
            .items
            .push(ConversationItem::ProviderItem { item: item.clone() });
        let body = responses_body(&request);
        assert_eq!(body.pointer("/input/0"), Some(&item));
    }
}
