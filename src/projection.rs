//! TUI 状态投影：v2 核心不向消费端暴露内部 runtime，TUI 通过协议事件与
//! snapshot/messages 增量维护当前会话的展示状态（entries、thinking、工具卡片、
//! approval、todo、滚动与布局），并向核心上报所有变更操作。

use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use protium_core::{
    commands::AgentMode,
    model::{
        AgentPhase, DisplayContent, DisplayEntry, DisplayKind, ModelPhase, ThinkingDisplay,
        ThinkingResult, TodoStatus, TodoTask, ToolDisplay, ToolDisplayStatus,
    },
    protocol::{Event, MessageDto},
    provider::{ToolCall, Usage},
    secrets,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::output::{CachedMarkdown, EdgeScroll, MessageLayout, OutputSelection};

/// A pending approval surfaced to the TUI. Only the consumer-facing id, the
/// tool call and the reason cross the boundary; the oneshot sender lives in
/// the core's runtime.
#[derive(Clone, Debug)]
pub struct ApprovalDisplay {
    pub approval_id: String,
    pub call: ToolCall,
    pub reason: String,
    pub source_session_id: Option<String>,
    pub source_title: Option<String>,
    pub created_at: Instant,
}

/// Outcome of applying one protocol event to the projection.
#[derive(Debug, Default)]
pub struct ProjectionOutcome {
    /// The session list (or the active session) may have changed; the caller
    /// should refresh the snapshot.
    pub sessions_dirty: bool,
    /// An approval overlay was dismissed and the frame needs a full redraw.
    pub force_redraw: bool,
    /// A transcript-changing event arrived; the caller must refetch the
    /// message page to rebuild history from the database.
    pub transcript_dirty: bool,
}

#[derive(Debug, Default)]
pub(crate) struct LiveThinkingLayoutCache {
    pub width: usize,
    pub source_start: usize,
    pub processed_len: usize,
    pub buffer_epoch: u64,
    pub rows: Vec<String>,
    pub current_row: String,
    pub current_width: usize,
    #[cfg(test)]
    pub full_rebuilds: usize,
    #[cfg(test)]
    pub processed_bytes: usize,
}

impl LiveThinkingLayoutCache {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Display state of one session, maintained by the TUI from v2 events and
/// message pages. The projection deliberately mirrors the display-facing
/// surface the renderer used to read off `SessionRuntime`, so `ui.rs` keeps
/// reading compatible fields; core-owned state (conversation, runners,
/// approvals, tool registry) never appears here.
#[derive(Debug)]
pub struct TuiSessionProjection {
    pub session_id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub entries: Vec<DisplayEntry>,
    pub todos: Vec<TodoTask>,
    pub todo_collapsed: bool,
    pub todo_hidden: bool,
    pub busy: bool,
    pub agent_phase: AgentPhase,
    pub model_phase: ModelPhase,
    pub status: String,
    pub mode: AgentMode,
    pub child_role: Option<String>,
    pub thinking_active: bool,
    pub thinking_last_line: String,
    pub thinking_buffer: String,
    pub thinking_buffer_truncated: bool,
    pub thinking_buffer_epoch: u64,
    pub thinking_result: ThinkingResult,
    pub thinking_animation_frame: usize,
    pub thinking_anchor: Option<usize>,
    pub thinking_expanded: bool,
    pub(crate) live_thinking_layout_cache: LiveThinkingLayoutCache,
    pub pending_approval: Option<ApprovalDisplay>,
    pub usage: Usage,
    pub context_used_tokens: u64,
    pub context_limit_tokens: Option<u64>,
    pub expanded_tools: HashSet<String>,
    pub expanded_thinking: HashSet<String>,
    pub message_scroll: usize,
    pub follow_output: bool,
    pub output_scroll_top: Option<usize>,
    pub output_selection: Option<OutputSelection>,
    pub message_layout: Option<MessageLayout>,
    pub markdown_render_cache: HashMap<usize, CachedMarkdown>,
    pub output_layout_dirty: bool,
    #[cfg(test)]
    pub output_layout_rebuild_count: usize,
    #[cfg(test)]
    pub markdown_parse_count: usize,
    #[cfg(test)]
    pub footer_rebuild_count: usize,
    pub edge_scroll: EdgeScroll,
}

pub(crate) const MAX_THINKING_LINE_BYTES: usize = 1024;
pub(crate) const MAX_THINKING_BUFFER_BYTES: usize = 64 * 1024;

impl TuiSessionProjection {
    /// A fresh, empty projection for a session (used before the first message
    /// page arrives).
    pub fn new(session_id: String, mode: AgentMode, context_limit_tokens: Option<u64>) -> Self {
        Self {
            session_id,
            title: String::new(),
            parent_id: None,
            entries: Vec::new(),
            todos: Vec::new(),
            todo_collapsed: false,
            todo_hidden: false,
            busy: false,
            agent_phase: AgentPhase::Idle,
            model_phase: ModelPhase::Idle,
            status: String::new(),
            mode,
            child_role: None,
            thinking_active: false,
            thinking_last_line: String::new(),
            thinking_buffer: String::new(),
            thinking_buffer_truncated: false,
            thinking_buffer_epoch: 0,
            thinking_result: ThinkingResult::Completed,
            thinking_animation_frame: 0,
            thinking_anchor: None,
            thinking_expanded: false,
            live_thinking_layout_cache: LiveThinkingLayoutCache::default(),
            pending_approval: None,
            usage: Usage::default(),
            context_used_tokens: 0,
            context_limit_tokens,
            expanded_tools: HashSet::new(),
            expanded_thinking: HashSet::new(),
            message_scroll: 0,
            follow_output: true,
            output_scroll_top: None,
            output_selection: None,
            message_layout: None,
            markdown_render_cache: HashMap::new(),
            output_layout_dirty: true,
            #[cfg(test)]
            output_layout_rebuild_count: 0,
            #[cfg(test)]
            markdown_parse_count: 0,
            #[cfg(test)]
            footer_rebuild_count: 0,
            edge_scroll: EdgeScroll::default(),
        }
    }

    pub fn set_todos(&mut self, tasks: Vec<TodoTask>) {
        self.todo_hidden = false;
        self.todo_collapsed =
            !tasks.is_empty() && tasks.iter().all(|task| task.status == TodoStatus::Done);
        self.todos = tasks;
    }

    /// Applies a routed protocol event to the projection. Returns what the
    /// caller (the facade) should do next.
    pub fn handle_event(&mut self, event: &Event) -> ProjectionOutcome {
        let mut outcome = ProjectionOutcome::default();
        match event {
            Event::ReasoningDelta { delta } => {
                self.agent_phase = AgentPhase::Thinking;
                self.model_phase = ModelPhase::Streaming;
                self.update_thinking_line(delta);
            }
            Event::ModelStreaming => {
                self.begin_thinking();
                self.agent_phase = AgentPhase::Thinking;
                self.model_phase = ModelPhase::Streaming;
                self.status = "等待模型流式响应".into();
            }
            Event::ProviderRetry {
                attempt,
                reason,
                delay_ms,
            } => {
                let delay_seconds = delay_ms.div_ceil(1000);
                self.status =
                    format!("请求失败，{delay_seconds} 秒后第 {attempt} 次重试（{reason}）");
            }
            Event::TodoUpdated { tasks } => {
                self.set_todos(tasks.clone());
            }
            Event::CompactionStarted => {
                self.status = "正在压缩上下文…… | Esc 取消".into();
                self.agent_phase = AgentPhase::Thinking;
                self.model_phase = ModelPhase::Streaming;
            }
            Event::CompactionCompleted { hidden } => {
                self.status = format!("上下文已压缩，隐藏 {hidden} 条历史消息");
            }
            Event::CompactionFailed { error } => {
                self.status = format!("上下文压缩失败，已使用安全裁剪：{error}");
                self.push_entry(DisplayEntry {
                    kind: DisplayKind::Error,
                    content: DisplayContent::Markdown(self.status.clone()),
                });
            }
            Event::WebSearchStarted { query } => {
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
            Event::WebSearchResult {
                title,
                url,
                snippet,
            } => {
                let context = format!("{title}\n{url}\n{snippet}");
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
                        result.push_str("\n\n");
                    }
                    result.push_str(&context);
                }
            }
            Event::WebSearchCompleted { count } => {
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
                self.status = if *count == 0 {
                    "联网搜索完成".into()
                } else {
                    format!("联网搜索完成：{count} 条结果")
                };
            }
            Event::Cancelled { reason } => {
                self.finish_thinking("思考已取消");
                self.busy = false;
                if self.pending_approval.is_some() {
                    outcome.force_redraw = true;
                }
                self.pending_approval = None;
                self.agent_phase = AgentPhase::Idle;
                self.model_phase = ModelPhase::Idle;
                self.status = if reason.contains("approval") {
                    "审批等待已取消".into()
                } else {
                    "请求已取消".into()
                };
            }
            Event::TextDelta { delta } => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::StreamingText;
                self.model_phase = ModelPhase::Streaming;
                self.invalidate_output_layout();
                if let Some(entry) = self.entries.last_mut()
                    && matches!(entry.kind, DisplayKind::Assistant)
                    && let DisplayContent::Markdown(text) = &mut entry.content
                {
                    text.push_str(delta);
                } else {
                    self.push_entry(DisplayEntry {
                        kind: DisplayKind::Assistant,
                        content: DisplayContent::Markdown(delta.clone()),
                    });
                }
                self.status = "正在输出正文…… | Esc 取消".into();
            }
            Event::Approval {
                approval_id,
                call,
                reason,
                source_session_id,
                source_title,
            } => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::WaitingApproval;
                self.model_phase = ModelPhase::Idle;
                self.status = "需要确认工具权限".into();
                self.pending_approval = Some(ApprovalDisplay {
                    approval_id: approval_id.clone(),
                    call: call.clone(),
                    reason: reason.clone(),
                    source_session_id: source_session_id.clone(),
                    source_title: source_title.clone(),
                    created_at: Instant::now(),
                });
            }
            Event::ApprovalResolved { approval_id, .. } => {
                if self
                    .pending_approval
                    .as_ref()
                    .is_some_and(|approval| approval.approval_id == *approval_id)
                {
                    self.pending_approval = None;
                    outcome.force_redraw = true;
                }
            }
            Event::ToolStarted { call } => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::ToolRunning;
                self.model_phase = ModelPhase::Idle;
                self.status = format!("正在执行 {}……", tool_display_name(&call.name));
                self.push_entry(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::Tool(ToolDisplay {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        status: ToolDisplayStatus::Running,
                        result: None,
                    }),
                });
            }
            Event::ToolFinished { call, result } => {
                self.agent_phase = AgentPhase::Thinking;
                let status = tool_result_status(result);
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
                    tool.result = Some(result.clone());
                    self.invalidate_output_layout();
                } else {
                    self.push_entry(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::Tool(ToolDisplay {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            status,
                            result: Some(result.clone()),
                        }),
                    });
                }
                self.status = "正在将工具结果交给模型……".into();
            }
            Event::Usage {
                input_tokens,
                output_tokens,
                total_tokens,
            } => {
                self.usage = Usage {
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                    total_tokens: *total_tokens,
                };
                if let Some(limit) = self.context_limit_tokens {
                    self.context_used_tokens = (*input_tokens).max(limit);
                } else {
                    self.context_used_tokens = *input_tokens;
                }
            }
            Event::Completed => {
                self.finish_thinking("思考完成");
                self.busy = false;
                self.agent_phase = AgentPhase::Idle;
                self.model_phase = ModelPhase::Completed;
                self.status = "就绪".into();
                outcome.sessions_dirty = true;
                outcome.transcript_dirty = true;
            }
            Event::SessionsChanged => {
                if !self.busy {
                    self.status = "会话列表已更新".into();
                }
                outcome.sessions_dirty = true;
            }
            Event::ChildSessionProgress { .. } => {}
            Event::Failed { error } => {
                self.finish_thinking("思考失败");
                self.push_entry(DisplayEntry {
                    kind: DisplayKind::Error,
                    content: DisplayContent::Markdown(secrets::redact(error)),
                });
                self.busy = false;
                self.agent_phase = AgentPhase::Failed;
                self.model_phase = ModelPhase::Failed;
                self.status = "请求失败".into();
            }
            Event::LocalCommandFinished { command, result } => {
                if command == "/diff" {
                    self.push_entry(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::Diff(result.clone()),
                    });
                    self.busy = false;
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
                        status: tool_result_status(result),
                        result: Some(result.clone()),
                    }),
                });
                self.busy = false;
                self.agent_phase = AgentPhase::Idle;
                self.model_phase = ModelPhase::Completed;
                self.status = "Shell 命令已完成".into();
            }
            Event::TranscriptInvalidated => {
                outcome.transcript_dirty = true;
            }
            Event::ResyncRequired => {
                outcome.sessions_dirty = true;
                outcome.transcript_dirty = true;
            }
        }
        self.trim_entries();
        outcome
    }

    fn begin_thinking(&mut self) {
        self.invalidate_output_layout();
        self.thinking_active = true;
        self.thinking_last_line = "模型正在思考".into();
        self.thinking_buffer.clear();
        self.thinking_buffer_truncated = false;
        self.thinking_buffer_epoch = self.thinking_buffer_epoch.wrapping_add(1);
        self.live_thinking_layout_cache.clear();
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

    /// Turns the buffered reasoning into a persistent thinking summary entry
    /// and retires the live row.
    fn persist_thinking_summary(&mut self) {
        let truncated = self.thinking_buffer_truncated;
        let reasoning = self.thinking_buffer.trim().to_owned();
        self.thinking_buffer.clear();
        self.thinking_last_line.clear();
        self.thinking_anchor = None;
        self.thinking_buffer_truncated = false;
        self.thinking_buffer_epoch = self.thinking_buffer_epoch.wrapping_add(1);
        self.live_thinking_layout_cache.clear();
        if reasoning.is_empty() {
            return;
        }
        let content = if truncated {
            format!("[较早思考内容已截断]\n\n{reasoning}")
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
        self.thinking_buffer_epoch = self.thinking_buffer_epoch.wrapping_add(1);
        self.live_thinking_layout_cache.clear();
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
            self.thinking_buffer_epoch = self.thinking_buffer_epoch.wrapping_add(1);
        }
        let latest = self
            .thinking_buffer
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("思考中");
        self.thinking_last_line = utf8_tail(latest, MAX_THINKING_LINE_BYTES).to_owned();
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
        self.markdown_render_cache.clear();
        self.thinking_anchor = self
            .thinking_anchor
            .map(|anchor| anchor.saturating_sub(removed));
        self.clear_output_selection();
    }

    pub fn scroll_messages(&mut self, delta: isize) -> bool {
        let previous = (
            self.message_scroll,
            self.follow_output,
            self.output_scroll_top,
        );
        let Some(layout) = &self.message_layout else {
            if delta > 0 {
                self.message_scroll = self.message_scroll.saturating_add(delta as usize);
                self.follow_output = false;
            } else {
                self.message_scroll = self.message_scroll.saturating_sub(delta.unsigned_abs());
                if self.message_scroll == 0 {
                    self.follow_output = true;
                }
            }
            return previous
                != (
                    self.message_scroll,
                    self.follow_output,
                    self.output_scroll_top,
                );
        };
        let max_scroll = layout.max_scroll();
        let current = self
            .output_scroll_top
            .unwrap_or(layout.scroll)
            .min(max_scroll);
        let next = next_output_scroll_top(current, max_scroll, delta);
        if delta < 0 && next == max_scroll {
            self.output_scroll_top = None;
            self.follow_output = true;
            self.message_scroll = 0;
        } else {
            self.output_scroll_top = Some(next);
            self.follow_output = false;
            self.message_scroll = max_scroll.saturating_sub(next);
        }
        previous
            != (
                self.message_scroll,
                self.follow_output,
                self.output_scroll_top,
            )
    }

    pub fn scroll_to_bottom(&mut self) {
        self.message_scroll = 0;
        self.follow_output = true;
        self.output_scroll_top = None;
    }

    /// Replaces the display history with one rebuilt from a message page, and
    /// drops transient streaming/thinking/scroll state.
    pub fn replace_history(&mut self, entries: Vec<DisplayEntry>) {
        self.entries = entries;
        self.invalidate_output_layout();
        self.markdown_render_cache.clear();
        self.clear_output_selection();
        self.thinking_anchor = None;
        self.scroll_to_bottom();
    }

    /// Converts a v2 message page into the display list, mirroring the core's
    /// `display_entries` mapping so history restored from the database matches
    /// the live streaming projection.
    pub fn message_dto_to_entries(messages: &[MessageDto]) -> Vec<DisplayEntry> {
        let mut entries = Vec::new();
        let mut tool_entries = HashMap::<String, usize>::new();
        let mut thinking_index = 0usize;
        for message in messages {
            match message {
                MessageDto::User { content, .. } => entries.push(DisplayEntry {
                    kind: DisplayKind::User,
                    content: DisplayContent::Markdown(content.clone()),
                }),
                MessageDto::Assistant { content, .. } => entries.push(DisplayEntry {
                    kind: DisplayKind::Assistant,
                    content: DisplayContent::Markdown(content.clone()),
                }),
                MessageDto::System { content, .. } => entries.push(DisplayEntry {
                    kind: DisplayKind::System,
                    content: DisplayContent::Markdown(content.clone()),
                }),
                MessageDto::Thinking { content, .. } => {
                    let id = format!("thinking-{thinking_index}");
                    thinking_index += 1;
                    entries.push(DisplayEntry {
                        kind: DisplayKind::Thinking,
                        content: DisplayContent::Thinking(ThinkingDisplay {
                            id,
                            content: content.clone(),
                        }),
                    });
                }
                MessageDto::Context { label, content, .. } => entries.push(DisplayEntry {
                    kind: DisplayKind::System,
                    content: DisplayContent::Markdown(format!("### @{label}\n\n{content}")),
                }),
                MessageDto::CompactionSummary { content, .. } => entries.push(DisplayEntry {
                    kind: DisplayKind::System,
                    content: DisplayContent::Markdown(format!("上下文压缩摘要\n\n{content}")),
                }),
                MessageDto::Tool {
                    call_id,
                    name,
                    arguments,
                    status,
                    result,
                    ..
                } => entries.push(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::Tool(ToolDisplay {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                        status: status_from_wire(status),
                        result: result.clone(),
                    }),
                }),
                MessageDto::ToolCalls { calls, .. } => {
                    for call in calls {
                        tool_entries.insert(call.id.clone(), entries.len());
                        entries.push(DisplayEntry {
                            kind: DisplayKind::Tool,
                            content: DisplayContent::Tool(ToolDisplay {
                                call_id: call.id.clone(),
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                                status: ToolDisplayStatus::Running,
                                result: None,
                            }),
                        });
                    }
                }
                MessageDto::ToolOutput {
                    call_id, output, ..
                } => {
                    if let Some(tool) = tool_entries
                        .get(call_id)
                        .and_then(|index| entries.get_mut(*index))
                        .and_then(|entry| match &mut entry.content {
                            DisplayContent::Tool(tool) => Some(tool),
                            _ => None,
                        })
                    {
                        tool.status = tool_result_status(output);
                        tool.result = Some(output.clone());
                    } else {
                        entries.push(DisplayEntry {
                            kind: DisplayKind::Tool,
                            content: DisplayContent::Tool(ToolDisplay {
                                call_id: call_id.clone(),
                                name: "tool".into(),
                                arguments: serde_json::Value::Null,
                                status: tool_result_status(output),
                                result: Some(output.clone()),
                            }),
                        });
                    }
                }
            }
        }
        if entries.is_empty() {
            entries.push(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown("1H-Agent 已就绪，请输入任务并按 Enter。".into()),
            });
        }
        entries
    }
}

fn status_from_wire(status: &str) -> ToolDisplayStatus {
    match status {
        "running" => ToolDisplayStatus::Running,
        "failed" => ToolDisplayStatus::Failed,
        "rejected" => ToolDisplayStatus::Rejected,
        _ => ToolDisplayStatus::Completed,
    }
}

/// Localized display name for a tool, used in status lines.
pub fn tool_display_name(name: &str) -> String {
    let translated = match name {
        "file_list" => Some("文件列表"),
        "file_stat" => Some("文件信息"),
        "file_read" => Some("文件读取"),
        "file_search" => Some("文件搜索"),
        "file_glob" => Some("文件查找"),
        "repo_map" => Some("符号大纲"),
        "file_mkdir" => Some("新建目录"),
        "file_write" => Some("文件修改"),
        "file_edit" => Some("文件编辑"),
        "file_copy" => Some("文件复制"),
        "file_move" => Some("文件移动"),
        "file_delete" => Some("文件删除"),
        "web_search" => Some("网络搜索"),
        "web_fetch" | "webfetch" => Some("网页读取"),
        "terminal_exec" => Some("命令执行"),
        "terminal_shell" => Some("Shell 命令"),
        "agent_spawn" => Some("子 Agent"),
        "git" => Some("Git 操作"),
        "git_diff" => Some("差异查看"),
        "browser_open" => Some("打开网页"),
        "browser_snapshot" => Some("页面快照"),
        "browser_click" => Some("页面点击"),
        "browser_type" => Some("页面输入"),
        "browser_press" => Some("页面按键"),
        _ => None,
    };
    if let Some(translated) = translated {
        return translated.to_owned();
    }
    if let Some(external) = name.strip_prefix("mcp:") {
        let tool = external.rsplit([':', '/']).next().unwrap_or(external);
        return format!("外部工具：{}", tool.replace('_', " "));
    }
    name.replace('_', " ")
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

fn display_entry_bytes(entries: &[DisplayEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match &entry.content {
            DisplayContent::Markdown(text) | DisplayContent::Diff(text) => text.len() + 32,
            DisplayContent::Tool(tool) => {
                tool.call_id.len() + tool.name.len() + tool.arguments.to_string().len() + 64
            }
            DisplayContent::Thinking(thinking) => thinking.content.len() + 32,
        })
        .sum()
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

pub(crate) fn next_output_scroll_top(current: usize, max_scroll: usize, delta: isize) -> usize {
    let current = current.min(max_scroll);
    if delta > 0 {
        current.saturating_sub(delta as usize)
    } else {
        current.saturating_add(delta.unsigned_abs()).min(max_scroll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_page_rebuilds_entry_history() {
        let messages = vec![
            MessageDto::User {
                id: 1,
                content: "hi".into(),
                created_at: "t".into(),
            },
            MessageDto::Assistant {
                id: 2,
                content: "hello".into(),
                created_at: "t".into(),
            },
            MessageDto::Thinking {
                id: 3,
                content: "reasoning".into(),
                created_at: "t".into(),
            },
        ];
        let entries = TuiSessionProjection::message_dto_to_entries(&messages);
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0].kind, DisplayKind::User));
        assert!(matches!(entries[1].kind, DisplayKind::Assistant));
        assert!(matches!(entries[2].kind, DisplayKind::Thinking));
    }

    #[test]
    fn text_delta_appends_to_live_assistant_entry() {
        let mut projection = TuiSessionProjection::new("s1".into(), AgentMode::Build, None);
        projection.handle_event(&Event::ModelStreaming);
        projection.handle_event(&Event::TextDelta {
            delta: "hel".into(),
        });
        projection.handle_event(&Event::TextDelta { delta: "lo".into() });
        assert_eq!(projection.entries.len(), 1);
        assert!(matches!(projection.entries[0].kind, DisplayKind::Assistant));
        assert!(
            matches!(&projection.entries[0].content, DisplayContent::Markdown(t) if t == "hello")
        );
    }

    #[test]
    fn tool_lifecycle_renders_a_card_and_finishes() {
        let mut projection = TuiSessionProjection::new("s1".into(), AgentMode::Build, None);
        let call = ToolCall {
            id: "c1".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
        };
        projection.handle_event(&Event::ToolStarted { call: call.clone() });
        projection.handle_event(&Event::ToolFinished {
            call,
            result: "ok".into(),
        });
        assert_eq!(projection.entries.len(), 1);
        assert!(matches!(
            &projection.entries[0].content,
            DisplayContent::Tool(tool) if tool.status == ToolDisplayStatus::Completed && tool.result.as_deref() == Some("ok")
        ));
    }

    #[test]
    fn approval_event_is_display_only() {
        let mut projection = TuiSessionProjection::new("s1".into(), AgentMode::Build, None);
        let call = ToolCall {
            id: "c1".into(),
            name: "file_write".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
        };
        projection.handle_event(&Event::Approval {
            approval_id: "ap1".into(),
            call: call.clone(),
            reason: "need".into(),
            source_session_id: None,
            source_title: None,
        });
        assert!(projection.pending_approval.is_some());
        projection.handle_event(&Event::ApprovalResolved {
            approval_id: "ap1".into(),
            approved: true,
        });
        assert!(projection.pending_approval.is_none());
    }
}
