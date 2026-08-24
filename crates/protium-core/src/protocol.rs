//! v2 UI wire protocol: stable DTOs shared by every interface.
//!
//! This module is the single authority for the UI contract. The [`Event`]
//! tagged union, [`AppSnapshotV2`], [`MessageDto`], [`MessagePage`] and
//! [`Envelope`] shapes are the stable wire types consumed by Web, TUI and
//! Desktop. `ts-rs` derives keep the generated TypeScript bindings in sync;
//! CI regenerates them and fails on drift (see the `protium-tsgen` binary).
//!
//! 对外契约，加法演进 (external contract, additive evolution): the `type`/`kind`
//! discriminator sets and field shapes are authoritative. New variants/fields
//! must be ignorable by older UIs; do not rename, reorder, or reuse an old tag
//! with a different payload without bumping [`PROTOCOL_VERSION`].

use serde::Serialize;
use serde_json::Value;
use ts_rs::TS;

use crate::{model::TodoTask, provider::ToolCall};

/// Current version of the UI wire protocol (REST + SSE DTOs).
pub const PROTOCOL_VERSION: u32 = 2;

/// Machine-readable error kind for the v2 API. [`ApiErrorKind::ResyncRequired`]
/// signals that a consumer's event cursor has been evicted from the bridge ring
/// and it must refetch the snapshot and the current message page instead of
/// guessing the missing state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorKind {
    BadRequest,
    NotFound,
    Unauthorized,
    Conflict,
    Internal,
    ResyncRequired,
}

/// A structured v2 API error. Serialized as `{ "kind", "message" }`.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            kind: ApiErrorKind::BadRequest,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: ApiErrorKind::NotFound,
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            kind: ApiErrorKind::Unauthorized,
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ApiErrorKind::Conflict,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ApiErrorKind::Internal,
            message: message.into(),
        }
    }

    pub fn resync_required() -> Self {
        Self {
            kind: ApiErrorKind::ResyncRequired,
            message: "event cursor was evicted; refetch the snapshot and message page".into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ApiError {}

/// One event payload delivered by the bridge. The `type` field is the
/// discriminator; the owning `session_id` and the process-global `cursor` live
/// on the [`Envelope`] wrapper.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ReasoningDelta {
        delta: String,
    },
    /// Marks the end of the current reasoning phase. Ordering guarantee within a
    /// model round: `ReasoningDelta* -> ReasoningCompleted -> TextDelta*`. It is
    /// emitted exactly once, only when the round produced reasoning, and is the
    /// render barrier consumers must use to commit/refresh the thinking view
    /// before accepting the first body delta. Old clients that ignore it still
    /// receive the same deltas and fall back to their existing behavior.
    ReasoningCompleted,
    ProviderRetry {
        attempt: u32,
        reason: String,
        delay_ms: u64,
    },
    ModelStreaming,
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
    Cancelled {
        reason: String,
    },
    TextDelta {
        delta: String,
    },
    /// Reports streaming progress while the model generates a tool call's
    /// arguments (a large `file_write` payload can take seconds). `received_bytes`
    /// is monotonic within one model round and the event is emitted at ~1 KiB
    /// thresholds, so consumers can animate a "generating tool call" row instead
    /// of freezing silently. Consumers may rely only on monotonicity, ordering
    /// and boundedness — the exact thresholds and event count are a core
    /// implementation detail and must not be depended on. Ordering guarantee
    /// within a model round: `ReasoningDelta* -> ReasoningCompleted -> TextDelta*
    /// -> ToolCallStreaming* -> Approval/ToolStarted`. Old clients that ignore it
    /// still receive the same deltas and fall back to their existing behavior.
    ToolCallStreaming {
        name: Option<String>,
        received_bytes: u64,
    },
    /// The agent is waiting for a tool approval. `approval_id` is the token the
    /// frontend must echo back to `POST /api/v2/approvals/:id`.
    Approval {
        approval_id: String,
        call: ToolCall,
        reason: String,
        source_session_id: Option<String>,
        source_title: Option<String>,
    },
    /// A previously broadcast approval was decided (by the user or by the
    /// server-side timeout). The frontend closes its modal on this.
    ApprovalResolved {
        approval_id: String,
        approved: bool,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        call: ToolCall,
        result: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
    },
    Completed,
    Failed {
        error: String,
    },
    SessionsChanged,
    ChildSessionProgress {
        child_session_id: String,
        status: String,
        turn: usize,
        max_turns: usize,
        tool: Option<String>,
    },
    LocalCommandFinished {
        command: String,
        result: String,
    },
    CompactionStarted,
    CompactionCompleted {
        hidden: usize,
    },
    CompactionFailed {
        error: String,
    },
    TodoUpdated {
        tasks: Vec<TodoTask>,
    },
    /// A history-modifying command (undo, redo, compact, fork, delete, rename,
    /// new session) changed the stored transcript. The consumer must drop its
    /// cached message pages and refetch from the newest page. Replaces the v1
    /// frontend's fixed-delay refresh.
    TranscriptInvalidated,
    /// The session's context budget changed (submit, usage, compaction, or a
    /// provider/model switch). The consumer updates its displayed safe-input
    /// budget; no history refetch is required.
    ContextUpdated {
        budget: ContextBudgetDto,
    },
    /// Transport-level signal: the consumer's event cursor was evicted from the
    /// bridge ring (or the consumer lagged the live channel). The consumer must
    /// refetch the snapshot and the current message page instead of guessing
    /// the missing state. `session_id` is empty on this envelope.
    ResyncRequired,
}

/// A single bridge-delivered event with its process-global monotonic cursor.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct Envelope {
    pub cursor: u64,
    pub session_id: String,
    #[serde(flatten)]
    pub event: Event,
}

/// Serialized form of a session used by [`AppSnapshotV2`].
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct SessionStateDto {
    pub id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub busy: bool,
    pub phase: String,
    pub status: String,
}

/// A pending approval exposed to the frontend. `approval_id` is echoed back on
/// decision; the server-side oneshot sender is never serialized.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct ApprovalDto {
    pub approval_id: String,
    pub session_id: String,
    pub call: ToolCall,
    pub reason: String,
    pub source_session_id: Option<String>,
    pub source_title: Option<String>,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct TodoDto {
    pub id: String,
    pub title: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&TodoTask> for TodoDto {
    fn from(task: &TodoTask) -> Self {
        Self {
            id: task.id.clone(),
            title: task.title.clone(),
            status: task.status.as_str().to_owned(),
            created_at: task.created_at.clone(),
            updated_at: task.updated_at.clone(),
        }
    }
}

/// Full application state snapshot returned by `GET /api/v2/state`.
///
/// `event_cursor` is the current global bridge cursor; the client subscribes
/// from this position so no event is lost between the snapshot and the SSE
/// stream. `protocol_version` lets a frontend reject an incompatible server
/// instead of misreading state. New fields must be additive.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct AppSnapshotV2 {
    pub protocol_version: u32,
    /// Current process-global event cursor; subscribe from here.
    pub event_cursor: u64,
    pub active_session: Option<String>,
    pub sessions: Vec<SessionStateDto>,
    pub provider: String,
    pub model: String,
    pub mode: String,
    /// Serialized pending approval of the oldest waiting session, if any.
    pub approval: Option<ApprovalDto>,
    pub todos: Vec<TodoDto>,
    /// Context capacity of the active session, computed by the core (the single
    /// authority). `None` when no session is active.
    pub context: Option<ContextBudgetDto>,
    /// The active session's persisted incomplete assistant answer, if any
    /// (survives a restart so an interrupted stream is still visible).
    pub assistant_partial: Option<PartialDto>,
}

/// A persisted incomplete assistant answer, shown as "未完成" and never fed
/// back into the model context or `previous_response_id`.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct PartialDto {
    pub content: String,
    pub created_at: String,
}

/// Per-session context capacity, computed by the core.
///
/// The core is the single authority for context capacity; the TUI must not
/// infer capacity from local character counts. `safe_input_tokens` is the
/// window minus the output reservation minus current usage — the budget a new
/// user message must fit into.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct ContextBudgetDto {
    /// The model's context window in tokens. `None` when the model is unknown
    /// and no explicit `context_window_tokens` is configured.
    pub context_window_tokens: Option<u64>,
    /// Current estimated used tokens for the session's conversation.
    pub used_tokens: u64,
    /// Output tokens reserved for the model's reply (the configured
    /// `max_output_tokens`, also the per-request provider hard cap).
    pub output_reserve_tokens: u64,
    /// Safe available input budget = window − reserve − used. `None` when the
    /// window is unknown.
    pub safe_input_tokens: Option<u64>,
    /// True when the window came from the built-in model registry (an
    /// estimate); false when it is an explicit user configuration.
    pub estimated: bool,
}

/// A single message in a session's transcript, in display shape. The `kind`
/// field is the discriminator; provider-private payloads are translated into a
/// display-safe shape and never leaked as raw JSON.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageDto {
    User {
        id: i64,
        content: String,
        created_at: String,
    },
    Assistant {
        id: i64,
        content: String,
        created_at: String,
    },
    System {
        id: i64,
        content: String,
        created_at: String,
    },
    Thinking {
        id: i64,
        content: String,
        created_at: String,
    },
    Context {
        id: i64,
        label: String,
        content: String,
        created_at: String,
    },
    CompactionSummary {
        id: i64,
        content: String,
        created_at: String,
    },
    Tool {
        id: i64,
        call_id: String,
        name: String,
        #[ts(type = "any")]
        arguments: Value,
        status: String,
        result: Option<String>,
        created_at: String,
    },
    ToolCalls {
        id: i64,
        calls: Vec<ToolCall>,
        created_at: String,
    },
    ToolOutput {
        id: i64,
        call_id: String,
        output: String,
        created_at: String,
    },
}

/// One page of a session transcript, in display (oldest→newest) order.
///
/// `next_before` is an opaque cursor for fetching the previous (older) page:
/// echo it back as the `before` query parameter. `has_more` is true when older
/// messages exist.
#[derive(Clone, Debug, Serialize, TS)]
#[ts(export)]
pub struct MessagePage {
    pub messages: Vec<MessageDto>,
    pub next_before: Option<i64>,
    pub has_more: bool,
}

/// The default page size for the messages endpoint; clamped to 20..=200.
pub const DEFAULT_PAGE_SIZE: usize = 100;

/// Lower bound for a message page size.
pub const MIN_PAGE_SIZE: usize = 20;

/// Upper bound for a message page size.
pub const MAX_PAGE_SIZE: usize = 200;

/// Clamps a requested page size to the protocol bounds.
pub fn clamp_page_size(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(MIN_PAGE_SIZE, MAX_PAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_variant_serializes_with_a_snake_case_tag() {
        let call = ToolCall {
            id: "c".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
        };
        let variants: Vec<Event> = vec![
            Event::ReasoningDelta { delta: "d".into() },
            Event::ProviderRetry {
                attempt: 1,
                reason: "r".into(),
                delay_ms: 100,
            },
            Event::ModelStreaming,
            Event::WebSearchStarted { query: "q".into() },
            Event::WebSearchResult {
                title: "t".into(),
                url: "u".into(),
                snippet: "sn".into(),
            },
            Event::WebSearchCompleted { count: 1 },
            Event::Cancelled { reason: "r".into() },
            Event::TextDelta { delta: "d".into() },
            Event::ToolCallStreaming {
                name: Some("file_write".into()),
                received_bytes: 9216,
            },
            Event::ReasoningCompleted,
            Event::Approval {
                approval_id: "ap".into(),
                call: call.clone(),
                reason: "r".into(),
                source_session_id: None,
                source_title: None,
            },
            Event::ApprovalResolved {
                approval_id: "ap".into(),
                approved: true,
            },
            Event::ToolStarted { call: call.clone() },
            Event::ToolFinished {
                call: call.clone(),
                result: "ok".into(),
            },
            Event::Usage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
            },
            Event::Completed,
            Event::Failed { error: "e".into() },
            Event::SessionsChanged,
            Event::ChildSessionProgress {
                child_session_id: "c".into(),
                status: "running".into(),
                turn: 1,
                max_turns: 2,
                tool: None,
            },
            Event::LocalCommandFinished {
                command: "c".into(),
                result: "r".into(),
            },
            Event::CompactionStarted,
            Event::CompactionCompleted { hidden: 1 },
            Event::CompactionFailed { error: "e".into() },
            Event::TodoUpdated { tasks: Vec::new() },
            Event::TranscriptInvalidated,
            Event::ContextUpdated {
                budget: ContextBudgetDto {
                    context_window_tokens: Some(128_000),
                    used_tokens: 1000,
                    output_reserve_tokens: 8192,
                    safe_input_tokens: Some(118_808),
                    estimated: true,
                },
            },
            Event::ResyncRequired,
        ];
        assert!(!variants.is_empty());
        for variant in variants {
            let json = serde_json::to_value(variant).expect("event serializes");
            let tag = json["type"].as_str().expect("event must carry a type tag");
            assert!(
                tag.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    && tag.chars().next().is_some_and(|c| c.is_ascii_lowercase()),
                "type tag must be snake_case, got {tag:?}"
            );
        }
    }

    #[test]
    fn tool_call_streaming_serializes_with_tag_and_fields() {
        let event = Event::ToolCallStreaming {
            name: Some("file_write".into()),
            received_bytes: 9216,
        };
        let json = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(json["type"], "tool_call_streaming");
        assert_eq!(json["name"], "file_write");
        assert_eq!(json["received_bytes"], 9216);
        // `name` is optional on the wire; old consumers ignore the event anyway.
        let unnamed = Event::ToolCallStreaming {
            name: None,
            received_bytes: 600,
        };
        let json = serde_json::to_value(unnamed).expect("event serializes");
        assert_eq!(json["type"], "tool_call_streaming");
        assert!(json.get("name").and_then(|value| value.as_str()).is_none());
        assert_eq!(json["received_bytes"], 600);
    }

    #[test]
    fn envelope_carries_cursor_session_and_flattened_event() {
        let envelope = Envelope {
            cursor: 7,
            session_id: "s1".into(),
            event: Event::TextDelta { delta: "hi".into() },
        };
        let json = serde_json::to_value(envelope).unwrap();
        assert_eq!(json["cursor"], 7);
        assert_eq!(json["session_id"], "s1");
        assert_eq!(json["type"], "text_delta");
        assert_eq!(json["delta"], "hi");
    }

    #[test]
    fn page_size_is_clamped_to_protocol_bounds() {
        assert_eq!(clamp_page_size(None), 100);
        assert_eq!(clamp_page_size(Some(10)), 20);
        assert_eq!(clamp_page_size(Some(500)), 200);
        assert_eq!(clamp_page_size(Some(50)), 50);
    }
}
