use std::time::Instant;

use serde_json::Value;
use tokio::sync::oneshot;

use crate::provider::ToolCall;

#[derive(Clone, Debug)]
pub enum DisplayKind {
    User,
    Assistant,
    Thinking,
    Tool,
    Error,
    System,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentPhase {
    #[default]
    Idle,
    Thinking,
    StreamingText,
    WaitingApproval,
    ToolRunning,
    Completed,
    Failed,
}

impl AgentPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Thinking => "THINKING",
            Self::StreamingText => "STREAMING_TEXT",
            Self::WaitingApproval => "WAITING_APPROVAL",
            Self::ToolRunning => "TOOL_RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModelPhase {
    #[default]
    Idle,
    Streaming,
    Completed,
    Failed,
}

impl ModelPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Streaming => "STREAMING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DisplayEntry {
    pub kind: DisplayKind,
    pub content: DisplayContent,
}

#[derive(Clone, Debug)]
pub enum DisplayContent {
    Markdown(String),
    Diff(String),
    Tool(ToolDisplay),
    Thinking(ThinkingDisplay),
}

#[derive(Clone, Debug)]
pub struct ThinkingDisplay {
    pub id: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDisplayStatus {
    Running,
    Completed,
    Failed,
    Rejected,
}

#[derive(Clone, Debug)]
pub struct ToolDisplay {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub status: ToolDisplayStatus,
    pub result: Option<String>,
}

pub struct PendingApproval {
    pub call: ToolCall,
    pub reason: String,
    pub source_session_id: Option<String>,
    pub source_title: Option<String>,
    pub action: ApprovalAction,
    pub created_at: Instant,
}

pub enum ApprovalAction {
    Agent(oneshot::Sender<bool>),
    Shell(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThinkingResult {
    #[default]
    Completed,
    Failed,
    Cancelled,
}
