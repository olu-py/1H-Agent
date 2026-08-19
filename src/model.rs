use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::provider::ToolCall;

#[derive(Clone, Debug)]
pub enum DisplayKind {
    Todo,
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
    Todo(TodoDisplay),
    Diff(String),
    Tool(ToolDisplay),
    Thinking(ThinkingDisplay),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Pending => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Pending,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Pending => "○",
            Self::InProgress => "◐",
            Self::Done => "●",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoTask {
    pub id: String,
    pub title: String,
    pub status: TodoStatus,
    pub created_at: String,
    pub updated_at: String,
}

impl TodoTask {
    pub fn new(title: impl Into<String>, status: TodoStatus) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            status,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoDisplay {
    pub tasks: Vec<TodoTask>,
}

impl TodoDisplay {
    pub fn progress(&self) -> (usize, usize) {
        (
            self.tasks
                .iter()
                .filter(|task| task.status == TodoStatus::Done)
                .count(),
            self.tasks.len(),
        )
    }
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
