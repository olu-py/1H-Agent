mod openai;

pub use openai::{OpenAiClient, SseDecoder};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::config::ProviderKind;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationItem {
    Message { role: Role, content: String },
    ThinkingSummary { content: String },
    Context { label: String, content: String },
    ProviderItem { item: Value },
    AssistantToolCalls { calls: Vec<ToolCall> },
    ToolOutput { call_id: String, output: String },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub kind: ProviderKind,
    pub model: String,
    pub items: Vec<ConversationItem>,
    pub tools: Vec<ToolDefinition>,
    pub previous_response_id: Option<String>,
    pub native_web_search: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    WebSearchStarted {
        query: String,
    },
    WebSearchResult {
        title: String,
        url: String,
        snippet: String,
    },
    WebSearchCompleted {
        count: usize,
    },
    ProviderItem(Value),
    TextDelta(String),
    ToolCallDelta {
        slot: String,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    ToolCallComplete(ToolCall),
    ResponseId(String),
    Usage(Usage),
    Done,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned HTTP {status}: {message}")]
    Status { status: u16, message: String },
    #[error("invalid provider event: {0}")]
    Protocol(String),
    #[error("model event receiver closed")]
    ReceiverClosed,
}
