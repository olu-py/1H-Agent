use std::collections::HashSet;

use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    agent::{AgentEvent, AgentRunner},
    commands::AgentMode,
    model::{AgentPhase, DisplayEntry, ModelPhase, PendingApproval, ThinkingResult},
    output::{EdgeScroll, MessageLayout, OutputSelection},
    provider::{ConversationItem, Usage},
};

/// Per-session runtime state. Each open session owns its own conversation,
/// display entries, agent runner, and streaming/thinking state, so multiple
/// agents can run in the background while the user switches between sessions.
pub struct SessionRuntime {
    pub session_id: String,
    pub entries: Vec<DisplayEntry>,
    pub busy: bool,
    pub agent_phase: AgentPhase,
    pub model_phase: ModelPhase,
    pub thinking_last_line: String,
    pub thinking_active: bool,
    pub thinking_buffer: String,
    pub thinking_buffer_truncated: bool,
    pub thinking_animation_frame: usize,
    pub thinking_anchor: Option<usize>,
    pub(crate) thinking_result: ThinkingResult,
    pub usage: Usage,
    pub context_used_tokens: u64,
    pub context_limit_tokens: Option<u64>,
    pub pending_approval: Option<PendingApproval>,
    pub mode: AgentMode,
    pub expanded_tools: HashSet<String>,
    pub expanded_thinking: HashSet<String>,
    pub thinking_expanded: bool,
    pub message_scroll: usize,
    pub follow_output: bool,
    pub output_scroll_top: Option<usize>,
    pub output_selection: Option<OutputSelection>,
    pub message_layout: Option<MessageLayout>,
    pub output_layout_dirty: bool,
    #[cfg(test)]
    pub output_layout_rebuild_count: usize,
    #[cfg(test)]
    pub markdown_parse_count: usize,
    #[cfg(test)]
    pub footer_rebuild_count: usize,
    pub edge_scroll: EdgeScroll,
    pub conversation: Vec<ConversationItem>,
    pub runner: Option<AgentRunner>,
    pub agent_tx: mpsc::Sender<AgentEvent>,
    pub active_task: Option<JoinHandle<()>>,
}
