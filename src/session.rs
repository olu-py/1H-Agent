use std::{collections::HashSet, path::Path, time::Instant};

use tokio::{sync::mpsc, task::JoinHandle};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    agent::{AgentEvent, AgentRunner},
    commands::AgentMode,
    model::{
        AgentPhase, ApprovalAction, DisplayContent, DisplayEntry, DisplayKind, ModelPhase,
        PendingApproval, ThinkingDisplay, ThinkingResult, ToolDisplay, ToolDisplayStatus,
    },
    output::{EdgeScroll, MessageLayout, OutputSelection},
    provider::{ConversationItem, Role, Usage},
    secrets,
    storage::Storage,
    ui,
};

/// Read-only context available while handling a per-session agent event.
pub struct EventCtx<'a> {
    pub storage: &'a Storage,
    pub workspace: &'a Path,
}

/// Outcome of applying a single agent event to a session.
#[derive(Debug, Default)]
pub struct SessionOutcome {
    /// The session list may have changed; the caller should refresh it.
    pub sessions_dirty: bool,
    /// An approval overlay was dismissed and the frame needs a full redraw.
    pub force_redraw: bool,
}

/// Per-session runtime state. Each open session owns its own conversation,
/// display entries, agent runner, and streaming/thinking state, so multiple
/// agents can run in the background while the user switches between sessions.
pub struct SessionRuntime {
    pub session_id: String,
    pub status: String,
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

impl SessionRuntime {
    /// Applies a single agent event to this session's runtime state. Session
    /// list refreshes are deferred to the caller via SessionOutcome, so this
    /// method only touches per-session state plus ctx.storage.
    pub fn handle_event(&mut self, ctx: &EventCtx<'_>, event: AgentEvent) -> SessionOutcome {
        let mut outcome = SessionOutcome::default();
        match event {
            AgentEvent::ReasoningDelta(delta) => {
                self.agent_phase = AgentPhase::Thinking;
                self.model_phase = ModelPhase::Streaming;
                self.update_thinking_line(&delta);
            }
            AgentEvent::ModelStreaming => {
                self.begin_thinking();
                self.agent_phase = AgentPhase::Thinking;
                self.model_phase = ModelPhase::Streaming;
                self.status = "等待模型流式响应".into();
            }
            AgentEvent::WebSearchStarted { query } => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::ToolRunning;
                self.model_phase = ModelPhase::Streaming;
                let already_open = self.entries.last().is_some_and(|entry| {
                    matches!(&entry.content, DisplayContent::Tool(tool) if tool.name == "web_search" && tool.status == ToolDisplayStatus::Running)
                });
                if !already_open {
                    let call_id = format!("native-web-search-{}", uuid::Uuid::new_v4());
                    self.push_entry(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::Tool(ToolDisplay {
                            call_id,
                            name: "web_search".into(),
                            arguments: serde_json::json!({"query": query}),
                            status: ToolDisplayStatus::Running,
                            result: None,
                        }),
                    });
                }
                self.status = "正在联网搜索".into();
            }
            AgentEvent::WebSearchResult {
                title,
                url,
                snippet,
            } => {
                let context = format!(
                    "{title}
{url}
{snippet}"
                );
                if let Some(tool) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match &mut entry.content {
                        DisplayContent::Tool(tool)
                            if tool.name == "web_search"
                                && tool.status == ToolDisplayStatus::Running =>
                        {
                            Some(tool)
                        }
                        _ => None,
                    })
                {
                    let result = tool.result.get_or_insert_with(String::new);
                    if !result.is_empty() {
                        result.push_str(
                            "

",
                        );
                    }
                    result.push_str(&context);
                }
            }
            AgentEvent::WebSearchCompleted { count } => {
                if let Some(tool) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match &mut entry.content {
                        DisplayContent::Tool(tool)
                            if tool.name == "web_search"
                                && tool.status == ToolDisplayStatus::Running =>
                        {
                            Some(tool)
                        }
                        _ => None,
                    })
                {
                    tool.status = ToolDisplayStatus::Completed;
                    self.invalidate_output_layout();
                }
                self.agent_phase = AgentPhase::Thinking;
                self.status = if count == 0 {
                    "联网搜索完成".into()
                } else {
                    format!("联网搜索完成：{count} 条结果")
                };
            }
            AgentEvent::Cancelled(reason) => {
                self.finish_thinking("思考已取消");
                self.busy = false;
                self.active_task = None;
                if let Some(approval) = self.take_pending_approval() {
                    outcome.force_redraw = true;
                    if let ApprovalAction::Agent(reply) = approval.action {
                        let _ = reply.send(false);
                    }
                }
                self.agent_phase = AgentPhase::Idle;
                self.model_phase = ModelPhase::Idle;
                self.status = if reason.contains("approval") {
                    "审批等待已取消".into()
                } else {
                    "请求已取消".into()
                };
            }
            AgentEvent::TextDelta(delta) => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::StreamingText;
                self.model_phase = ModelPhase::Streaming;
                self.invalidate_output_layout();
                if let Some(entry) = self.entries.last_mut()
                    && matches!(entry.kind, DisplayKind::Assistant)
                    && let DisplayContent::Markdown(text) = &mut entry.content
                {
                    text.push_str(&delta);
                } else {
                    self.push_entry(DisplayEntry {
                        kind: DisplayKind::Assistant,
                        content: DisplayContent::Markdown(delta),
                    });
                }
                self.status = "正在输出正文…… | Esc 取消".into();
            }
            AgentEvent::Approval {
                call,
                reason,
                reply,
            } => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::WaitingApproval;
                self.model_phase = ModelPhase::Idle;
                self.status = "需要确认工具权限".into();
                self.pending_approval = Some(PendingApproval {
                    call,
                    reason,
                    action: ApprovalAction::Agent(reply),
                    created_at: Instant::now(),
                });
            }
            AgentEvent::ToolStarted(call) => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::ToolRunning;
                self.model_phase = ModelPhase::Idle;
                self.status = format!("正在执行 {}……", ui::tool_display_name(&call.name));
                self.push_entry(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::Tool(ToolDisplay {
                        call_id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                        status: ToolDisplayStatus::Running,
                        result: None,
                    }),
                });
            }
            AgentEvent::ToolFinished { call, result } => {
                self.agent_phase = AgentPhase::Thinking;
                let status = tool_result_status(&result);
                if let Some(tool) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match &mut entry.content {
                        DisplayContent::Tool(tool) if tool.call_id == call.id => Some(tool),
                        _ => None,
                    })
                {
                    tool.status = status;
                    tool.result = Some(result);
                    self.invalidate_output_layout();
                } else {
                    self.push_entry(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::Tool(ToolDisplay {
                            call_id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                            status,
                            result: Some(result),
                        }),
                    });
                }
                self.status = "正在将工具结果交给模型……".into();
            }
            AgentEvent::Usage(usage) => {
                self.context_used_tokens = usage
                    .input_tokens
                    .max(estimate_context_tokens(&self.conversation));
                self.usage = usage;
            }
            AgentEvent::Completed { items } => {
                self.finish_thinking("思考完成");
                self.conversation = items;
                trim_conversation(&mut self.conversation);
                self.busy = false;
                self.active_task = None;
                self.agent_phase = AgentPhase::Idle;
                self.model_phase = ModelPhase::Completed;
                self.status = "就绪".into();
                outcome.sessions_dirty = true;
            }
            AgentEvent::SessionsChanged => {
                if !self.busy {
                    self.status = "会话列表已更新".into();
                }
                outcome.sessions_dirty = true;
            }
            AgentEvent::Failed(error) => {
                self.finish_thinking("思考失败");
                self.push_entry(DisplayEntry {
                    kind: DisplayKind::Error,
                    content: DisplayContent::Markdown(secrets::redact(&error)),
                });
                self.busy = false;
                self.active_task = None;
                self.agent_phase = AgentPhase::Failed;
                self.model_phase = ModelPhase::Failed;
                self.status = "请求失败".into();
            }
            AgentEvent::LocalCommandFinished { command, result } => {
                if command == "/diff" {
                    self.push_entry(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::Diff(result),
                    });
                    self.busy = false;
                    self.active_task = None;
                    self.agent_phase = AgentPhase::Idle;
                    self.model_phase = ModelPhase::Completed;
                    self.status = "Git diff 已准备好".into();
                    self.trim_entries();
                    return outcome;
                }
                self.push_entry(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::Tool(ToolDisplay {
                        call_id: format!("local-shell-{}", uuid::Uuid::new_v4()),
                        name: "terminal_shell".into(),
                        arguments: serde_json::json!({"command": command}),
                        status: tool_result_status(&result),
                        result: Some(result.clone()),
                    }),
                });
                self.conversation.push(ConversationItem::Context {
                    label: format!("shell: {command}"),
                    content: result.clone(),
                });
                if let Err(error) = ctx.storage.append_context(
                    &self.session_id,
                    &format!("shell: {command}"),
                    &result,
                ) {
                    self.status = format!("命令已完成，但保存失败：{error}");
                } else {
                    self.status = "Shell 命令已完成".into();
                }
                self.busy = false;
                self.active_task = None;
                self.agent_phase = AgentPhase::Idle;
                self.model_phase = ModelPhase::Completed;
            }
        }
        self.trim_entries();
        outcome
    }

    fn begin_thinking(&mut self) {
        // Anchor the single live row before the next entry. TextDelta will append
        // that assistant entry at the same index, so the row never moves.
        self.invalidate_output_layout();
        self.thinking_active = true;
        self.thinking_last_line = "模型正在思考".into();
        self.thinking_buffer.clear();
        self.thinking_buffer_truncated = false;
        self.thinking_animation_frame = 0;
        self.thinking_anchor = Some(self.entries.len());
        self.thinking_expanded = false;
    }

    pub fn finish_thinking(&mut self, line: &str) {
        self.thinking_active = false;
        self.thinking_animation_frame = 0;
        self.persist_thinking_summary();
        match line {
            "思考失败" => self.thinking_result = ThinkingResult::Failed,
            "思考已取消" => self.thinking_result = ThinkingResult::Cancelled,
            _ => self.thinking_result = ThinkingResult::Completed,
        }
    }

    /// Turns the buffered reasoning into a persistent "思考摘要" entry so every
    /// thinking round is kept in the task stream instead of being overwritten by
    /// the next round. The live row is then retired (anchor cleared).
    fn persist_thinking_summary(&mut self) {
        let truncated = self.thinking_buffer_truncated;
        let reasoning = self.thinking_buffer.trim().to_owned();
        self.thinking_buffer.clear();
        self.thinking_last_line.clear();
        self.thinking_anchor = None;
        self.thinking_buffer_truncated = false;
        if reasoning.is_empty() {
            return;
        }
        let content = if truncated {
            format!(
                "[较早思考内容已截断]

{reasoning}"
            )
        } else {
            reasoning
        };
        self.push_entry(DisplayEntry {
            kind: DisplayKind::Thinking,
            content: DisplayContent::Thinking(ThinkingDisplay {
                id: format!("thinking-{}", uuid::Uuid::new_v4()),
                content,
            }),
        });
    }

    pub fn reset_thinking_state(&mut self) {
        self.thinking_active = false;
        self.thinking_last_line.clear();
        self.thinking_buffer.clear();
        self.thinking_buffer_truncated = false;
        self.thinking_animation_frame = 0;
        self.thinking_anchor = None;
        self.thinking_result = ThinkingResult::Completed;
    }

    fn update_thinking_line(&mut self, delta: &str) {
        self.thinking_active = true;
        self.thinking_buffer.push_str(delta);
        if self.thinking_buffer.len() > MAX_THINKING_BUFFER_BYTES {
            let minimum = self
                .thinking_buffer
                .len()
                .saturating_sub(MAX_THINKING_BUFFER_BYTES);
            let start = self
                .thinking_buffer
                .grapheme_indices(true)
                .map(|(offset, _)| offset)
                .find(|offset| *offset >= minimum)
                .unwrap_or(self.thinking_buffer.len());
            self.thinking_buffer.drain(..start);
            self.thinking_buffer_truncated = true;
        }
        let latest = self
            .thinking_buffer
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("思考中");
        self.thinking_last_line = utf8_tail(latest, MAX_THINKING_LINE_BYTES).to_owned();
    }

    pub fn take_pending_approval(&mut self) -> Option<PendingApproval> {
        self.pending_approval.take()
    }

    pub fn push_entry(&mut self, entry: DisplayEntry) {
        self.clear_output_selection();
        self.invalidate_output_layout();
        self.entries.push(entry);
    }

    pub fn clear_output_selection(&mut self) {
        self.output_selection = None;
        self.edge_scroll = EdgeScroll::default();
    }

    pub fn invalidate_output_layout(&mut self) {
        self.message_layout.take();
        self.output_layout_dirty = true;
    }

    pub fn trim_entries(&mut self) {
        const MAX_ENTRIES: usize = 1000;
        const MAX_BYTES: usize = 2 * 1024 * 1024;
        if self.entries.len() <= MAX_ENTRIES && display_entry_bytes(&self.entries) <= MAX_BYTES {
            return;
        }
        self.invalidate_output_layout();
        let removed = trim_entries(&mut self.entries);
        self.thinking_anchor = self
            .thinking_anchor
            .map(|anchor| anchor.saturating_sub(removed));
        self.clear_output_selection();
    }
}

pub(crate) const MAX_THINKING_LINE_BYTES: usize = 1024;
pub(crate) const MAX_THINKING_BUFFER_BYTES: usize = 64 * 1024;

fn utf8_tail(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let minimum = value.len().saturating_sub(max_bytes);
    let start = value
        .grapheme_indices(true)
        .map(|(offset, _)| offset)
        .find(|offset| *offset >= minimum)
        .unwrap_or(value.len());
    &value[start..]
}

pub(crate) fn tool_result_status(result: &str) -> ToolDisplayStatus {
    let lower = result.to_ascii_lowercase();
    if lower.starts_with("rejected by user") || lower.starts_with("denied by policy") {
        ToolDisplayStatus::Rejected
    } else if lower.starts_with("tool failed")
        || lower.starts_with("security policy denied")
        || lower.starts_with("process timed out")
        || lower.starts_with("duplicate tool call")
    {
        ToolDisplayStatus::Failed
    } else {
        ToolDisplayStatus::Completed
    }
}

pub(crate) fn trim_entries(entries: &mut Vec<DisplayEntry>) -> usize {
    const MAX_ENTRIES: usize = 1000;
    const MAX_BYTES: usize = 2 * 1024 * 1024;
    let mut removed = 0;
    while entries.len() > MAX_ENTRIES || display_entry_bytes(entries) > MAX_BYTES {
        if entries.len() > MAX_ENTRIES {
            let count = entries.len() - MAX_ENTRIES;
            entries.drain(..count);
            removed += count;
        } else {
            entries.remove(0);
            removed += 1;
        }
    }
    removed
}

pub(crate) fn trim_conversation(items: &mut Vec<ConversationItem>) {
    const MAX_ITEMS: usize = 200;
    const MAX_BYTES: usize = 1024 * 1024;
    let mut removed = 0usize;
    while items.len() > MAX_ITEMS || conversation_bytes(items) > MAX_BYTES {
        if items.is_empty() {
            break;
        }
        items.remove(0);
        removed += 1;
    }
    if removed > 0 {
        items.insert(
            0,
            ConversationItem::Message {
                role: Role::System,
                content: format!(
                    "Earlier context was locally compacted ({removed} items omitted)."
                ),
            },
        );
    }
}

fn conversation_bytes(items: &[ConversationItem]) -> usize {
    items
        .iter()
        .map(|item| match item {
            ConversationItem::Message { content, .. }
            | ConversationItem::Context { content, .. } => content.len(),
            ConversationItem::ThinkingSummary { .. } => 0,
            ConversationItem::ProviderItem { item } => item.to_string().len(),
            ConversationItem::AssistantToolCalls { calls } => calls
                .iter()
                .map(|call| call.name.len() + call.arguments.to_string().len())
                .sum(),
            ConversationItem::ToolOutput { output, .. } => output.len(),
        })
        .sum()
}

pub(crate) fn estimate_context_tokens(items: &[ConversationItem]) -> u64 {
    let bytes = conversation_bytes(items) as u64;
    // A conservative, allocation-free estimate for mixed prose/JSON context.
    (bytes.saturating_add(3) / 4).max(1)
}

fn display_entry_bytes(entries: &[DisplayEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match &entry.content {
            DisplayContent::Markdown(value) => value.len(),
            DisplayContent::Diff(value) => value.len(),
            DisplayContent::Tool(tool) => {
                tool.call_id.len()
                    + tool.name.len()
                    + tool.arguments.to_string().len()
                    + tool.result.as_ref().map_or(0, String::len)
            }
            DisplayContent::Thinking(thinking) => thinking.id.len() + thinking.content.len(),
        })
        .sum()
}
