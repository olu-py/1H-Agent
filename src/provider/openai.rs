use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{
    ConversationItem, ModelEvent, ModelRequest, ProviderError, Role, ToolCall, ToolDefinition,
    Usage,
};
use crate::config::ProviderKind;

#[derive(Clone)]
pub struct OpenAiClient {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
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
        })
    }

    pub async fn stream(
        &self,
        request: ModelRequest,
        events: mpsc::Sender<ModelEvent>,
    ) -> Result<(), ProviderError> {
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
                    ProviderKind::Responses => parse_responses_event(&value),
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
    body
}

fn chat_item(item: &ConversationItem) -> Option<Value> {
    match item {
        ConversationItem::Message { role, content } => Some(json!({
            "role": role_name(*role),
            "content": content,
        })),
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
    let input: Vec<Value> = request.items.iter().flat_map(responses_item).collect();
    let tools: Vec<Value> = request.tools.iter().map(responses_tool).collect();
    let mut body = json!({
        "model": request.model,
        "input": input,
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(id) = &request.previous_response_id {
        body["previous_response_id"] = Value::String(id.clone());
    }
    body
}

fn responses_item(item: &ConversationItem) -> Vec<Value> {
    match item {
        ConversationItem::Message { role, content } => vec![json!({
            "role": role_name(*role),
            "content": content,
        })],
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
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
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
    Ok(events)
}

pub(crate) fn parse_responses_event(value: &Value) -> Result<Vec<ModelEvent>, ProviderError> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut events = Vec::new();
    match event_type {
        "response.created" | "response.in_progress" => {
            if let Some(id) = value.pointer("/response/id").and_then(Value::as_str) {
                events.push(ModelEvent::ResponseId(id.to_owned()));
            }
        }
        "response.output_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ModelEvent::TextDelta(delta.to_owned()));
            }
        }
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
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
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
    fn parses_responses_text() {
        let events = parse_responses_event(&json!({
            "type":"response.output_text.delta", "delta":"hello"
        }))
        .unwrap();
        assert_eq!(events, vec![ModelEvent::TextDelta("hello".into())]);
    }
}
