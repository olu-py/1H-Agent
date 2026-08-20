mod openai;

pub use openai::{OpenAiClient, SseDecoder};

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::config::{ProviderKind, ThinkingLevel, ThinkingProfileKind};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationItem {
    Message { role: Role, content: String },
    ThinkingSummary { content: String },
    CompactionSummary { content: String },
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
    pub thinking_mode: ThinkingMode,
    pub thinking_level: ThinkingLevel,
    pub thinking_budget_tokens: Option<u32>,
    pub thinking_profile_kind: ThinkingProfileKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    OpenAiResponsesSummary,
    DeepSeekResponses,
    QwenResponses,
    DeepSeekChat,
    QwenChat,
    VolcanoChat,
    CompatibleAuto,
    #[default]
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    ReasoningDelta(String),
    Retrying {
        attempt: u32,
        reason: String,
        delay_ms: u64,
    },
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
    Status {
        status: u16,
        message: String,
        retry_after_ms: Option<u64>,
    },
    #[error("invalid provider event: {0}")]
    Protocol(String),
    #[error("model event receiver closed")]
    ReceiverClosed,
}

/// HTTP-level retry decision. Returns a backoff delay when the failure is safe
/// to retry before any event has been emitted; `None` means not retryable.
///
/// - Connect/request (send-phase) failures and retryable status codes
///   (408/429/500/502/503/504) may be retried.
/// - A provider `Retry-After` hint wins over exponential backoff and is clamped
///   to `max_ms`.
/// - Protocol errors, non-retryable statuses, and `ReceiverClosed` (cancellation)
///   are never retried.
pub(crate) fn retry_delay(
    error: &ProviderError,
    attempt: u32,
    initial_ms: u64,
    max_ms: u64,
) -> Option<Duration> {
    let delay_ms = match error {
        ProviderError::Http(http) if http.is_connect() || http.is_request() => {
            Some(exponential_backoff(attempt, initial_ms, max_ms))
        }
        ProviderError::Status {
            status,
            retry_after_ms,
            ..
        } if matches!(*status, 408 | 429 | 500 | 502 | 503 | 504) => Some(
            retry_after_ms
                .unwrap_or_else(|| exponential_backoff(attempt, initial_ms, max_ms))
                .min(max_ms),
        ),
        _ => None,
    };
    delay_ms.map(Duration::from_millis)
}

fn exponential_backoff(attempt: u32, initial_ms: u64, max_ms: u64) -> u64 {
    initial_ms
        .saturating_mul(1_u64 << attempt.saturating_sub(1).min(20))
        .min(max_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(status: u16, retry_after_ms: Option<u64>) -> ProviderError {
        ProviderError::Status {
            status,
            message: "boom".into(),
            retry_after_ms,
        }
    }

    #[test]
    fn retry_delay_retries_retryable_statuses_with_exponential_backoff() {
        for status_code in [408, 429, 500, 502, 503, 504] {
            assert!(
                retry_delay(&status(status_code, None), 1, 500, 8000).is_some(),
                "status {status_code} should be retryable"
            );
        }
        assert_eq!(
            retry_delay(&status(429, None), 1, 500, 8000),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            retry_delay(&status(429, None), 2, 500, 8000),
            Some(Duration::from_millis(1000))
        );
        assert_eq!(
            retry_delay(&status(429, None), 5, 500, 8000),
            Some(Duration::from_millis(8000))
        );
    }

    #[test]
    fn retry_delay_prefers_retry_after_clamped_to_max() {
        assert_eq!(
            retry_delay(&status(429, Some(2000)), 1, 500, 8000),
            Some(Duration::from_millis(2000))
        );
        assert_eq!(
            retry_delay(&status(503, Some(60_000)), 3, 500, 8000),
            Some(Duration::from_millis(8000))
        );
    }

    #[test]
    fn retry_delay_rejects_non_retryable_statuses_and_protocol_errors() {
        assert_eq!(retry_delay(&status(400, None), 1, 500, 8000), None);
        assert_eq!(retry_delay(&status(401, None), 1, 500, 8000), None);
        assert_eq!(retry_delay(&status(404, None), 1, 500, 8000), None);
        assert_eq!(
            retry_delay(&ProviderError::Protocol("bad event".into()), 1, 500, 8000),
            None
        );
        assert_eq!(
            retry_delay(&ProviderError::ReceiverClosed, 1, 500, 8000),
            None
        );
    }

    #[test]
    fn retry_delay_never_retries_receiver_closed_cancellation() {
        assert_eq!(
            retry_delay(&ProviderError::ReceiverClosed, 1, 500, 8000),
            None
        );
        assert_eq!(
            retry_delay(
                &ProviderError::Status {
                    status: 429,
                    message: "cancelled".into(),
                    retry_after_ms: None,
                },
                1,
                500,
                8000
            ),
            Some(Duration::from_millis(500))
        );
    }
}
