use std::{collections::HashMap, path::Path, time::Instant};

use serde_json::Value;
use tokio::{sync::mpsc, task::JoinHandle};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    agent::{AgentEvent, AgentRunner},
    commands::AgentMode,
    model::{
        AgentPhase, ApprovalAction, DisplayContent, DisplayEntry, DisplayKind, ModelPhase,
        PendingApproval, ThinkingDisplay, ThinkingResult, TodoTask, ToolDisplay, ToolDisplayStatus,
    },
    provider::{ConversationItem, Role, Usage},
    secrets,
    storage::Storage,
};

/// Human-readable byte count for status lines (e.g. "9.0 KB").
pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Localized display name for a tool, used in status lines. UI-independent;
/// the TUI-specific display code no longer exists.
pub(crate) fn tool_display_name(name: &str) -> String {
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
}

/// Per-session runtime state. Each open session owns its own conversation,
/// display entries, agent runner, and streaming/thinking state, so multiple
/// agents can run in the background while the user switches between sessions.
pub struct SessionRuntime {
    pub session_id: String,
    pub status: String,
    pub entries: Vec<DisplayEntry>,
    pub todos: Vec<TodoTask>,
    pub busy: bool,
    pub agent_phase: AgentPhase,
    pub model_phase: ModelPhase,
    pub thinking_last_line: String,
    pub thinking_active: bool,
    pub thinking_buffer: String,
    pub thinking_buffer_truncated: bool,
    pub(crate) thinking_buffer_epoch: u64,
    pub(crate) thinking_result: ThinkingResult,
    pub usage: Usage,
    pub context_used_tokens: u64,
    pub context_limit_tokens: Option<u64>,
    pub pending_approval: Option<PendingApproval>,
    pub mode: AgentMode,
    pub child_role: Option<String>,
    pub conversation: Vec<ConversationItem>,
    pub runner: Option<AgentRunner>,
    pub agent_tx: mpsc::Sender<AgentEvent>,
    pub active_task: Option<JoinHandle<()>>,
    /// Monotonic per-session request sequence. Each `submit` that starts a task
    /// bumps it; `cancel` carries the expected sequence so a stale cancel (for a
    /// superseded request) is ignored instead of aborting a newer request.
    pub request_seq: u64,
    /// When the runtime was last parked into the background; used to evict the
    /// least-recently-parked idle runtime when background capacity is exceeded.
    pub parked_at: Instant,
}

impl SessionRuntime {
    pub(crate) fn set_todos(&mut self, tasks: Vec<TodoTask>) {
        self.todos = tasks;
    }

    /// Fully stops this runtime: aborts its in-flight agent task (which also
    /// tears down any nested child-agent futures) and rejects any approval
    /// still waiting on it. Used when deleting a session and when evicting or
    /// exiting background runtimes.
    pub(crate) fn shutdown(&mut self) {
        if let Some(approval) = self.take_pending_approval() {
            if let ApprovalAction::Agent(reply) = approval.action {
                let _ = reply.send(false);
            }
        }
        if let Some(task) = self.active_task.take() {
            task.abort();
        }
    }

    /// Whether this runtime can be evicted without interrupting work.
    pub(crate) fn idle(&self) -> bool {
        !self.busy && self.active_task.is_none() && self.pending_approval.is_none()
    }

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
            AgentEvent::ReasoningCompleted => {
                // Render barrier from the thinking view to the body view: the
                // buffered reasoning is persisted as a summary and the live
                // thinking row is retired before any body delta arrives.
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::StreamingText;
                self.model_phase = ModelPhase::Streaming;
                self.status = "正在输出正文…… | Esc 取消".into();
            }
            AgentEvent::ModelStreaming => {
                self.begin_thinking();
                self.agent_phase = AgentPhase::Thinking;
                self.model_phase = ModelPhase::Streaming;
                self.status = "等待模型流式响应".into();
            }
            AgentEvent::ProviderRetry {
                attempt,
                reason,
                delay_ms,
            } => {
                let delay_seconds = delay_ms.div_ceil(1000);
                self.status =
                    format!("请求失败，{delay_seconds} 秒后第 {attempt} 次重试（{reason}）");
            }
            AgentEvent::TodoUpdated { tasks } => {
                self.set_todos(tasks);
            }
            AgentEvent::CompactionStarted => {
                self.status = "正在压缩上下文…… | Esc 取消".into();
                self.agent_phase = AgentPhase::Thinking;
                self.model_phase = ModelPhase::Streaming;
            }
            AgentEvent::CompactionCompleted { hidden } => {
                self.status = format!("上下文已压缩，隐藏 {hidden} 条历史消息");
            }
            AgentEvent::CompactionFailed(error) => {
                self.status = format!("上下文压缩失败，已使用安全裁剪：{error}");
                self.push_entry(DisplayEntry {
                    kind: DisplayKind::Error,
                    content: DisplayContent::Markdown(self.status.clone()),
                });
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
                self.mark_partial_if_streaming();
                self.busy = false;
                self.active_task = None;
                if let Some(approval) = self.take_pending_approval() {
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
                if let Some(entry) = self.entries.last_mut()
                    && matches!(
                        entry.kind,
                        DisplayKind::Assistant | DisplayKind::AssistantPartial
                    )
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
            AgentEvent::ToolCallStreaming {
                name,
                received_bytes,
            } => {
                self.agent_phase = AgentPhase::StreamingToolCall;
                self.model_phase = ModelPhase::Streaming;
                let display = name
                    .as_deref()
                    .map(tool_display_name)
                    .unwrap_or_else(|| "工具调用".to_owned());
                self.status = format!(
                    "正在生成 {display} 调用参数（{}）……",
                    format_bytes(received_bytes)
                );
            }
            AgentEvent::Approval {
                call,
                reason,
                source_session_id,
                source_title,
                reply,
            } => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::WaitingApproval;
                self.model_phase = ModelPhase::Idle;
                self.status = "需要确认工具权限".into();
                self.pending_approval = Some(PendingApproval {
                    call,
                    reason,
                    source_session_id,
                    source_title,
                    action: ApprovalAction::Agent(reply),
                    created_at: Instant::now(),
                });
            }
            AgentEvent::ToolStarted(call) => {
                self.finish_thinking("思考完成");
                self.agent_phase = AgentPhase::ToolRunning;
                self.model_phase = ModelPhase::Idle;
                self.status = format!("正在执行 {}……", tool_display_name(&call.name));
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
                let compacted = items
                    .iter()
                    .any(|item| matches!(item, ConversationItem::CompactionSummary { .. }));
                self.conversation = items;
                // Known-window models rely on window-based capacity and
                // compaction; only apply the fixed item/byte ceiling as a
                // memory safety net when the window is unknown.
                if self.context_limit_tokens.is_none() {
                    trim_conversation(&mut self.conversation);
                }
                if compacted {
                    self.entries = display_entries(&self.conversation);
                }
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
            AgentEvent::ChildSessionProgress { .. } => {}
            AgentEvent::Failed(error) => {
                self.finish_thinking("思考失败");
                self.mark_partial_if_streaming();
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
        self.thinking_active = true;
        self.thinking_last_line = "模型正在思考".into();
        self.thinking_buffer.clear();
        self.thinking_buffer_truncated = false;
        self.thinking_buffer_epoch = self.thinking_buffer_epoch.wrapping_add(1);
    }

    pub fn finish_thinking(&mut self, line: &str) {
        self.thinking_active = false;
        self.persist_thinking_summary();
        match line {
            "思考失败" => self.thinking_result = ThinkingResult::Failed,
            "思考已取消" => self.thinking_result = ThinkingResult::Cancelled,
            _ => self.thinking_result = ThinkingResult::Completed,
        }
    }

    /// Turns the buffered reasoning into a persistent "思考摘要" entry so every
    /// thinking round is kept in the task stream instead of being overwritten by
    /// the next round.
    fn persist_thinking_summary(&mut self) {
        let truncated = self.thinking_buffer_truncated;
        let reasoning = self.thinking_buffer.trim().to_owned();
        self.thinking_buffer.clear();
        self.thinking_last_line.clear();
        self.thinking_buffer_truncated = false;
        self.thinking_buffer_epoch = self.thinking_buffer_epoch.wrapping_add(1);
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

    pub fn take_pending_approval(&mut self) -> Option<PendingApproval> {
        self.pending_approval.take()
    }

    /// Marks the trailing live assistant entry as incomplete when a stream is
    /// interrupted (cancelled or failed) before its answer was persisted. A
    /// persisted tool-round assistant entry is never touched (the last entry is
    /// then a tool, not an assistant).
    pub(crate) fn mark_partial_if_streaming(&mut self) {
        if let Some(entry) = self.entries.last_mut()
            && matches!(entry.kind, DisplayKind::Assistant)
            && matches!(&entry.content, DisplayContent::Markdown(t) if !t.trim().is_empty())
        {
            entry.kind = DisplayKind::AssistantPartial;
        }
    }

    pub fn push_entry(&mut self, entry: DisplayEntry) {
        self.entries.push(entry);
    }

    pub fn trim_entries(&mut self) {
        const MAX_ENTRIES: usize = 1000;
        const MAX_BYTES: usize = 2 * 1024 * 1024;
        if self.entries.len() <= MAX_ENTRIES && display_entry_bytes(&self.entries) <= MAX_BYTES {
            return;
        }
        trim_entries(&mut self.entries);
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
    trim_conversation_bounded(items, 200, 1024 * 1024);
}

/// Conservative token budget for the system prompt, tool schemas and stream
/// scaffolding. Subtracted from the model window when computing the safe input
/// capacity so requests never exceed the window on top of system overhead.
pub(crate) const SYSTEM_OVERHEAD_TOKENS: u64 = 4096;

/// The safe input token capacity for a provider: model window minus the output
/// reservation (configured `max_output_tokens`) minus system overhead. `None`
/// when the model window is unknown — the caller must not assume a capacity.
pub fn safe_input_capacity(provider: &crate::config::ProviderConfig) -> Option<u64> {
    let window = provider.resolved_context_window_tokens()?;
    let reserve = u64::from(provider.max_output_tokens.unwrap_or(0));
    Some(
        window
            .saturating_sub(reserve)
            .saturating_sub(SYSTEM_OVERHEAD_TOKENS),
    )
}

/// Hard-limit degradation: trims the conversation from the front until its
/// estimated token count fits `budget_tokens` (or nothing is left), then
/// inserts a localized note. Returns the number of items removed. This is the
/// last-resort fallback after compaction failed, never a silent pre-send trim.
pub fn trim_conversation_to_budget(items: &mut Vec<ConversationItem>, budget_tokens: u64) -> usize {
    let mut removed = 0usize;
    while !items.is_empty() {
        if estimate_context_tokens(items) <= budget_tokens {
            break;
        }
        items.remove(0);
        removed += 1;
    }
    while matches!(items.first(), Some(ConversationItem::ToolOutput { .. })) {
        items.remove(0);
        removed += 1;
    }
    if removed > 0 {
        items.insert(
            0,
            ConversationItem::Message {
                role: Role::System,
                content: format!(
                    "Earlier context was trimmed to fit the model's input budget ({removed} items omitted)."
                ),
            },
        );
    }
    removed
}

pub(crate) fn trim_conversation_bounded(
    items: &mut Vec<ConversationItem>,
    max_items: usize,
    max_bytes: usize,
) {
    let mut removed = 0usize;
    while items.len() > max_items || conversation_bytes(items) > max_bytes {
        if items.is_empty() {
            break;
        }
        items.remove(0);
        removed += 1;
    }
    while matches!(items.first(), Some(ConversationItem::ToolOutput { .. })) {
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
            ConversationItem::CompactionSummary { content } => content.len(),
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

pub(crate) fn display_entries(conversation: &[ConversationItem]) -> Vec<DisplayEntry> {
    let mut entries = Vec::new();
    let mut tool_entries = HashMap::<String, usize>::new();
    let mut thinking_index = 0usize;
    for item in conversation {
        match item {
            ConversationItem::Message { role, content } => entries.push(DisplayEntry {
                kind: match role {
                    Role::User => DisplayKind::User,
                    Role::Assistant => DisplayKind::Assistant,
                    Role::System => DisplayKind::System,
                },
                content: DisplayContent::Markdown(content.clone()),
            }),
            ConversationItem::ThinkingSummary { content } => {
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
            ConversationItem::CompactionSummary { content } => entries.push(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(format!("上下文压缩摘要\n\n{content}")),
            }),
            ConversationItem::Context { label, content } => entries.push(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(format!("### @{label}\n\n{content}")),
            }),
            ConversationItem::ProviderItem { item } => {
                if item.get("type").and_then(serde_json::Value::as_str) == Some("web_search_call") {
                    entries.push(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::Tool(ToolDisplay {
                            call_id: item
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("native-web-search-{}", entries.len())),
                            name: "web_search".into(),
                            arguments: item.get("action").cloned().unwrap_or_default(),
                            status: ToolDisplayStatus::Completed,
                            result: None,
                        }),
                    });
                }
            }
            ConversationItem::AssistantToolCalls { calls } => {
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
            ConversationItem::ToolOutput { call_id, output } => {
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
                            arguments: Value::Null,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderPreset;

    #[test]
    fn safe_input_capacity_subtracts_reserve_and_system_overhead() {
        let mut provider = ProviderPreset::DeepSeek.defaults();
        provider.model = "deepseek-v4-flash".into();
        assert_eq!(provider.resolved_context_window_tokens(), Some(1_000_000));
        provider.max_output_tokens = Some(8192);
        assert_eq!(
            safe_input_capacity(&provider),
            Some(1_000_000 - 8192 - SYSTEM_OVERHEAD_TOKENS)
        );
        // No output cap configured: only the system overhead is reserved.
        provider.max_output_tokens = None;
        assert_eq!(
            safe_input_capacity(&provider),
            Some(1_000_000 - SYSTEM_OVERHEAD_TOKENS)
        );
    }

    #[test]
    fn safe_input_capacity_is_none_for_unknown_windows() {
        let mut provider = ProviderPreset::Custom.defaults();
        provider.model = "some-brand-new-model".into();
        assert_eq!(provider.resolved_context_window_tokens(), None);
        assert_eq!(safe_input_capacity(&provider), None);
        // An explicit window restores a capacity.
        provider.context_window_tokens = Some(128_000);
        assert_eq!(
            safe_input_capacity(&provider),
            Some(128_000 - SYSTEM_OVERHEAD_TOKENS)
        );
    }

    #[test]
    fn trim_conversation_to_budget_trims_to_capacity_with_note() {
        let items = (0..40)
            .map(|i| ConversationItem::Message {
                role: Role::User,
                content: format!("message {i} padding padding padding"),
            })
            .collect::<Vec<_>>();
        let mut items = items;
        let before = items.len();
        let removed = trim_conversation_to_budget(&mut items, 64);
        assert!(removed > 0);
        assert!(removed < before);
        assert!(estimate_context_tokens(&items) <= 64 + 64);
        assert!(matches!(
            &items[0],
            ConversationItem::Message {
                role: Role::System,
                ..
            }
        ));
    }

    #[test]
    fn trim_conversation_to_budget_is_a_noop_within_budget() {
        let mut items = vec![ConversationItem::Message {
            role: Role::User,
            content: "small".into(),
        }];
        assert_eq!(trim_conversation_to_budget(&mut items, 1_000_000), 0);
        assert_eq!(items.len(), 1);
    }
}
