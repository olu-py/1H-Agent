use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, Semaphore, mpsc, oneshot},
    time::timeout,
};

use crate::{
    config::{
        AgentConfig, ClusterConfig, CompactionConfig, NativeWebSearch, ProviderConfig,
        ProviderKind, ProviderPreset, ThinkingCapability, thinking_profile,
    },
    prompt,
    provider::{
        ConversationItem, ModelEvent, ModelRequest, OpenAiClient, Role, ThinkingMode, ToolCall,
        ToolDefinition, Usage,
    },
    secrets,
    security::PolicyDecision,
    session::trim_conversation_bounded,
    storage::Storage,
    tools::SharedToolRegistry,
};

pub(crate) type ChildProviderResolver =
    dyn Fn(ProviderPreset) -> Result<ProviderConfig, String> + Send + Sync;

/// Upper bound for a single persisted thinking summary. Matches the UI's live
/// thinking buffer limit so the stored summary and the displayed summary stay
/// consistent even when the model streams an unusually long reasoning block.
const MAX_REASONING_BYTES: usize = 64 * 1024;

/// Appends a reasoning delta while keeping the buffer within
/// `MAX_REASONING_BYTES`, retaining the tail (like the live UI buffer) on
/// overflow.
fn append_reasoning_bounded(buffer: &mut String, delta: &str) {
    buffer.push_str(delta);
    if buffer.len() <= MAX_REASONING_BYTES {
        return;
    }
    let minimum = buffer.len() - MAX_REASONING_BYTES;
    let start = buffer
        .char_indices()
        .map(|(offset, _)| offset)
        .find(|offset| *offset >= minimum)
        .unwrap_or(buffer.len());
    buffer.drain(..start);
}

/// Appends `delta` to `buffer` without exceeding `max_bytes`, truncating at a
/// UTF-8 character boundary. Used for bounded child-agent output.
fn append_text_bounded(buffer: &mut String, delta: &str, max_bytes: usize) {
    let remaining = max_bytes.saturating_sub(buffer.len());
    if remaining == 0 {
        return;
    }
    let mut end = delta.len().min(remaining);
    while end > 0 && !delta.is_char_boundary(end) {
        end -= 1;
    }
    buffer.push_str(&delta[..end]);
}

/// Tools a child agent may ever receive. This is intentionally smaller than the
/// role-based filter: no terminal, shell, git mutation, browser, MCP, spawn, or
/// delete tools are ever delegated to a child.
fn child_tool_name_allowed(tool: &str, role: Option<&str>, allowed_tools: &[String]) -> bool {
    const READ_TOOLS: &[&str] = &[
        "file_list",
        "file_stat",
        "file_read",
        "file_search",
        "web_search",
        "web_fetch",
        "git_diff",
    ];
    const WRITE_TOOLS: &[&str] = &["file_write", "file_mkdir", "file_copy", "file_move"];

    if READ_TOOLS.contains(&tool) {
        return true;
    }
    if !allowed_tools.is_empty() {
        return WRITE_TOOLS.contains(&tool) && allowed_tools.iter().any(|name| name == tool);
    }
    WRITE_TOOLS.contains(&tool) && is_implement_role(role)
}

/// Truncates `value` to at most `max_bytes` at a UTF-8 character boundary,
/// appending a marker when bytes were dropped.
fn truncate_utf8_bounded(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[child output truncated]", &value[..end])
}

/// Maps a model id prefix to its canonical provider preset. Used both to infer
/// an omitted `provider` in `agent_spawn` and to catch an explicit provider
/// that contradicts the requested model (for example `provider=qwen` with
/// `model=deepseek-v4-flash`).
fn model_prefix_preset(model: &str) -> Option<ProviderPreset> {
    let model = model.trim().to_ascii_lowercase();
    if model.starts_with("deepseek") {
        Some(ProviderPreset::DeepSeek)
    } else if model.starts_with("qwen") {
        Some(ProviderPreset::Qwen)
    } else if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        Some(ProviderPreset::OpenAi)
    } else if model.starts_with("doubao") || model.starts_with("glm") {
        Some(ProviderPreset::Volcano)
    } else {
        None
    }
}

/// Chooses the child provider when `agent_spawn` omits the `provider`
/// argument. Current provider wins when it already lists the model; otherwise
/// an explicit family prefix (deepseek/qwen/gpt/o*/doubao/glm) infers the
/// matching provider, and finally any preset whose selectable list contains
/// the model is tried.
fn infer_child_provider(model: &str, current: ProviderPreset) -> ProviderPreset {
    if current == ProviderPreset::Custom {
        return current;
    }
    let model = model.trim().to_ascii_lowercase();
    if current.selectable_models().contains(&model.as_str()) {
        return current;
    }
    if let Some(preset) = model_prefix_preset(&model) {
        return preset;
    }
    for preset in ProviderPreset::ALL {
        if preset != current && preset.selectable_models().contains(&model.as_str()) {
            return preset;
        }
    }
    current
}

/// Validates a child model id without rejecting new provider models that are
/// absent from the built-in picker list (for example `qwen3.5-flash`). It only
/// rejects empty ids, obvious shorthands such as `v4pro`, and explicit
/// provider/model prefix contradictions.
fn validate_child_model(provider_config: &ProviderConfig) -> Result<(), String> {
    let model = provider_config.model.trim();
    if model.is_empty() {
        return Err("child agent model must not be empty".into());
    }
    if provider_config.preset == ProviderPreset::Custom {
        return Ok(());
    }
    let normalized = model.to_ascii_lowercase();
    let selectable = provider_config.preset.selectable_models();
    if selectable.contains(&normalized.as_str()) {
        return Ok(());
    }
    if let Some(prefix_preset) = model_prefix_preset(&normalized)
        && prefix_preset != provider_config.preset
    {
        return Err(format!(
            "model \"{model}\" belongs to {}; set provider={} or omit provider to infer it",
            prefix_preset.label(),
            prefix_preset.key_id()
        ));
    }
    let looks_like_full_id =
        normalized.contains('-') || normalized.contains('.') || normalized.contains(':');
    if !looks_like_full_id {
        return Err(format!(
            "unknown model \"{model}\" for {}; use a full model name such as {}",
            provider_config.preset.label(),
            selectable.join(", ")
        ));
    }
    Ok(())
}

fn child_title(
    arguments: &ChildArgs,
    configured_agent: Option<&AgentConfig>,
    role: Option<&str>,
) -> String {
    if let Some(title) = arguments
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return title.chars().take(80).collect();
    }
    if let Some(agent) = configured_agent {
        return agent.name.clone();
    }
    if let Some(role) = role.map(str::trim).filter(|r| !r.is_empty()) {
        let prompt = arguments.prompt.trim();
        let suffix = if prompt.chars().count() > 18 {
            prompt.chars().take(18).collect::<String>() + "…"
        } else {
            prompt.to_owned()
        };
        return format!("{role}·{suffix}");
    }
    "子 Agent".into()
}

/// Whether a child role implies write access. Planning/review roles stay
/// read-only; implementation/coding roles may write files (subject to the
/// normal approval policy).
fn is_implement_role(role: Option<&str>) -> bool {
    role.is_some_and(|role| {
        let role = role.to_ascii_lowercase();
        [
            "implement",
            "implementation",
            "code",
            "coder",
            "write",
            "build",
            "实施",
            "编码",
        ]
        .iter()
        .any(|keyword| role.contains(keyword))
    })
}

/// Summarizes a child agent's completed tool results so a turn-limited child
/// does not lose all its intermediate work. Returns the last `max_items`
/// results, each truncated to `max_bytes`.
fn summarize_child_trail(items: &[ConversationItem], max_items: usize, max_bytes: usize) -> String {
    let mut summary = String::new();
    let mut count = 0usize;
    for item in items.iter().rev() {
        let ConversationItem::ToolOutput { output, .. } = item else {
            continue;
        };
        if count >= max_items {
            break;
        }
        let output = output.trim();
        if output.is_empty() {
            continue;
        }
        count += 1;
        let end = output.len().min(max_bytes);
        let mut end = end;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        summary.insert_str(0, &format!("\n[tool result]: {}\n", &output[..end]));
    }
    summary
}

fn incremental_request_cursor(items: &[ConversationItem]) -> usize {
    items
        .iter()
        .rposition(|item| {
            matches!(
                item,
                ConversationItem::Message {
                    role: Role::User,
                    ..
                }
            )
        })
        .unwrap_or_else(|| items.len().saturating_sub(1))
}

/// Produces protocol-valid local history for stateless replay. Context
/// trimming or cancellation can leave one half of a tool call/output pair;
/// Responses endpoints reject either an orphan output or an unanswered call.
fn replay_safe_items(items: &[ConversationItem]) -> Vec<ConversationItem> {
    let output_ids = items
        .iter()
        .filter_map(|item| match item {
            ConversationItem::ToolOutput { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut known_calls = HashSet::<String>::new();
    let mut replay = Vec::with_capacity(items.len());
    for item in items {
        match item {
            ConversationItem::AssistantToolCalls { calls } => {
                let calls = calls
                    .iter()
                    .filter(|call| output_ids.contains(call.id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                if !calls.is_empty() {
                    known_calls.extend(calls.iter().map(|call| call.id.clone()));
                    replay.push(ConversationItem::AssistantToolCalls { calls });
                }
            }
            ConversationItem::ToolOutput { call_id, .. } if !known_calls.contains(call_id) => {}
            _ => replay.push(item.clone()),
        }
    }
    replay
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildSessionStatus {
    Queued,
    WaitingModel,
    Streaming,
    RunningTool,
    WaitingApprovalSlot,
    WaitingApproval,
    Completed,
    Failed,
    TurnLimit,
    TimedOut,
    Cancelled,
}

impl ChildSessionStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::TurnLimit | Self::TimedOut | Self::Cancelled
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::WaitingModel => "等待模型",
            Self::Streaming => "模型响应中",
            Self::RunningTool => "执行工具",
            Self::WaitingApprovalSlot => "等待审批槽",
            Self::WaitingApproval => "等待审批",
            Self::Completed => "完成",
            Self::Failed => "失败",
            Self::TurnLimit => "达到轮次上限",
            Self::TimedOut => "执行超时",
            Self::Cancelled => "已取消",
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TurnLimit => "turn_limit",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Queued
            | Self::WaitingModel
            | Self::Streaming
            | Self::RunningTool
            | Self::WaitingApprovalSlot
            | Self::WaitingApproval => "running",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildSessionProgress {
    pub status: ChildSessionStatus,
    pub turn: usize,
    pub max_turns: usize,
    pub tool: Option<String>,
    pub updated_at: Instant,
}

impl ChildSessionProgress {
    pub fn label(&self) -> String {
        let turn = (self.turn > 0).then(|| {
            if self.max_turns == 0 {
                format!(" 第{}轮", self.turn)
            } else {
                format!(" {}/{}", self.turn, self.max_turns)
            }
        });
        let tool = self.tool.as_deref().map(|name| format!(" ·{name}"));
        format!(
            "{}{}{}",
            self.status.label(),
            turn.unwrap_or_default(),
            tool.unwrap_or_default()
        )
    }
}

fn child_progress(
    status: ChildSessionStatus,
    turn: usize,
    max_turns: usize,
    tool: Option<String>,
) -> ChildSessionProgress {
    ChildSessionProgress {
        status,
        turn,
        max_turns,
        tool,
        updated_at: Instant::now(),
    }
}

async fn emit_child_progress(
    ui_events: &mpsc::Sender<AgentEvent>,
    session_id: &str,
    progress: ChildSessionProgress,
) {
    let _ = ui_events
        .send(AgentEvent::ChildSessionProgress {
            session_id: session_id.to_owned(),
            progress,
        })
        .await;
}

struct ChildCancellationGuard {
    ui_events: mpsc::Sender<AgentEvent>,
    session_id: String,
    max_turns: usize,
    finished: bool,
}

struct ChildToolContext<'a> {
    child_id: &'a str,
    child_title: &'a str,
    ui_events: &'a mpsc::Sender<AgentEvent>,
    turn: usize,
    max_turns: usize,
}

impl ChildCancellationGuard {
    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for ChildCancellationGuard {
    fn drop(&mut self) {
        if !self.finished {
            let event = AgentEvent::ChildSessionProgress {
                session_id: self.session_id.clone(),
                progress: child_progress(ChildSessionStatus::Cancelled, 0, self.max_turns, None),
            };
            if let Err(mpsc::error::TrySendError::Full(event)) = self.ui_events.try_send(event)
                && let Ok(runtime) = tokio::runtime::Handle::try_current()
            {
                let ui_events = self.ui_events.clone();
                runtime.spawn(async move {
                    let _ = ui_events.send(event).await;
                });
            }
        }
    }
}

#[derive(Debug)]
pub enum AgentEvent {
    ReasoningDelta(String),
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
    Cancelled(String),
    TextDelta(String),
    Approval {
        call: ToolCall,
        reason: String,
        source_session_id: Option<String>,
        source_title: Option<String>,
        reply: oneshot::Sender<bool>,
    },
    ToolStarted(ToolCall),
    ToolFinished {
        call: ToolCall,
        result: String,
    },
    Usage(Usage),
    Completed {
        items: Vec<ConversationItem>,
    },
    Failed(String),
    SessionsChanged,
    ChildSessionProgress {
        session_id: String,
        progress: ChildSessionProgress,
    },
    LocalCommandFinished {
        command: String,
        result: String,
    },
    CompactionStarted,
    CompactionCompleted {
        hidden: usize,
    },
    CompactionFailed(String),
}

#[derive(Clone)]
pub struct AgentRunner {
    provider: OpenAiClient,
    provider_config: ProviderConfig,
    tools: SharedToolRegistry,
    storage: Storage,
    session_id: String,
    approval_lock: Arc<Mutex<()>>,
    child_slots: Arc<Semaphore>,
    child_role: Option<String>,
    cluster: ClusterConfig,
    configured_agents: Arc<Vec<AgentConfig>>,
    child_provider_resolver: Option<Arc<ChildProviderResolver>>,
    compaction: CompactionConfig,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates the common per-round streaming state: assistant/reasoning text,
/// partial tool calls, and completed tool calls. The streaming loop and partial
/// convergence are shared by the main agent and child agents.
struct StreamCollector {
    assistant_text: String,
    reasoning_text: String,
    partials: HashMap<String, PartialToolCall>,
    completed_calls: Vec<ToolCall>,
    completed_ids: HashSet<String>,
    saw_done: bool,
    max_text_bytes: Option<usize>,
}

impl StreamCollector {
    fn new(max_text_bytes: Option<usize>) -> Self {
        Self {
            assistant_text: String::new(),
            reasoning_text: String::new(),
            partials: HashMap::new(),
            completed_calls: Vec::new(),
            completed_ids: HashSet::new(),
            saw_done: false,
            max_text_bytes,
        }
    }

    /// Accumulates text/tool-call state. Returns the event unchanged when it
    /// needs caller-level side effects (web search, provider items, usage,
    /// response id, or reasoning forwarding); returns None when fully handled.
    fn on_event(&mut self, event: ModelEvent) -> Option<ModelEvent> {
        match event {
            ModelEvent::TextDelta(delta) => {
                if let Some(max_bytes) = self.max_text_bytes {
                    append_text_bounded(&mut self.assistant_text, &delta, max_bytes);
                } else {
                    self.assistant_text.push_str(&delta);
                }
                Some(ModelEvent::TextDelta(delta))
            }
            ModelEvent::ReasoningDelta(delta) => {
                append_reasoning_bounded(&mut self.reasoning_text, &delta);
                Some(ModelEvent::ReasoningDelta(delta))
            }
            ModelEvent::ToolCallDelta {
                slot,
                id,
                name,
                arguments_delta,
            } => {
                let partial = self.partials.entry(slot).or_default();
                if let Some(id) = id {
                    partial.id = id;
                }
                if let Some(name) = name {
                    partial.name = name;
                }
                partial.arguments.push_str(&arguments_delta);
                None
            }
            ModelEvent::ToolCallComplete(call) => {
                self.completed_ids.insert(call.id.clone());
                self.completed_calls.push(call);
                None
            }
            ModelEvent::Done => {
                self.saw_done = true;
                None
            }
            other => Some(other),
        }
    }

    fn finish_partials(&mut self, error_kind: &str) -> Result<(), String> {
        for partial in std::mem::take(&mut self.partials).into_values() {
            if self.completed_ids.contains(&partial.id) || partial.name.is_empty() {
                continue;
            }
            let arguments: Value = serde_json::from_str(if partial.arguments.is_empty() {
                "{}"
            } else {
                &partial.arguments
            })
            .map_err(|error| format!("invalid {error_kind} arguments: {error}"))?;
            self.completed_calls.push(ToolCall {
                id: if partial.id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4())
                } else {
                    partial.id
                },
                name: partial.name,
                arguments,
            });
        }
        Ok(())
    }
}

/// What a forwarded stream event should do on the UI channel.
enum Forwarded {
    /// Send this agent event, propagating send failures.
    Send(AgentEvent),
    /// Send this agent event, ignoring send failures.
    SendIgnore(AgentEvent),
    /// The event was handled locally and needs no UI forwarding.
    Ignore,
}

/// Why a single model stream round failed.
enum StreamFailure {
    /// A caller-side event handler failed (fatal).
    Handler(String),
    /// The provider returned an error for this request (replayable when the
    /// round produced no output).
    Provider(String),
    /// The spawned provider task failed to join (fatal).
    Join(String),
    /// The stream ended without a Done marker (fatal).
    EndedWithoutCompletion,
}

/// Streams one model request into `collector`, forwarding events the collector
/// does not own to `forward`, then sending the resulting agent events to the UI.
async fn stream_once(
    provider: &OpenAiClient,
    request: ModelRequest,
    collector: &mut StreamCollector,
    channel_capacity: usize,
    ui_events: &mpsc::Sender<AgentEvent>,
    mut forward: impl FnMut(ModelEvent) -> Result<Forwarded, String>,
) -> Result<(), StreamFailure> {
    let (model_tx, mut model_rx) = mpsc::channel(channel_capacity);
    let provider = provider.clone();
    let provider_task = tokio::spawn(async move { provider.stream(request, model_tx).await });
    while let Some(event) = model_rx.recv().await {
        if let Some(event) = collector.on_event(event) {
            match forward(event).map_err(StreamFailure::Handler)? {
                Forwarded::Send(agent_event) => ui_events
                    .send(agent_event)
                    .await
                    .map_err(|_| StreamFailure::Handler("UI event receiver closed".to_owned()))?,
                Forwarded::SendIgnore(agent_event) => {
                    let _ = ui_events.send(agent_event).await;
                }
                Forwarded::Ignore => {}
            }
        }
        if collector.saw_done {
            break;
        }
    }
    let provider_result = provider_task
        .await
        .map_err(|error| StreamFailure::Join(error.to_string()))?;
    if let Err(error) = provider_result {
        return Err(StreamFailure::Provider(error.to_string()));
    }
    if !collector.saw_done {
        return Err(StreamFailure::EndedWithoutCompletion);
    }
    Ok(())
}

impl AgentRunner {
    pub fn new(
        provider: OpenAiClient,
        provider_config: ProviderConfig,
        tools: SharedToolRegistry,
        storage: Storage,
        session_id: String,
    ) -> Self {
        Self {
            provider,
            provider_config,
            tools,
            storage,
            session_id,
            approval_lock: Arc::new(Mutex::new(())),
            child_slots: Arc::new(Semaphore::new(4)),
            child_role: None,
            cluster: ClusterConfig::default(),
            configured_agents: Arc::new(Vec::new()),
            child_provider_resolver: None,
            compaction: CompactionConfig::default(),
        }
    }

    pub fn with_cluster_config(mut self, cluster: ClusterConfig) -> Self {
        self.child_slots = Arc::new(Semaphore::new(
            cluster.max_parallel_children.unwrap_or(4).clamp(1, 32),
        ));
        self.cluster = cluster;
        self
    }

    pub fn with_approval_lock(mut self, approval_lock: Arc<Mutex<()>>) -> Self {
        self.approval_lock = approval_lock;
        self
    }

    pub fn with_configured_agents(mut self, agents: Vec<AgentConfig>) -> Self {
        self.configured_agents = Arc::new(agents);
        self
    }

    pub fn with_child_role(mut self, child_role: Option<String>) -> Self {
        self.child_role = child_role;
        self
    }

    pub fn with_child_provider_resolver(mut self, resolver: Arc<ChildProviderResolver>) -> Self {
        self.child_provider_resolver = Some(resolver);
        self
    }

    pub fn with_compaction_config(mut self, config: CompactionConfig) -> Self {
        self.compaction = config;
        self.compaction.normalize();
        self
    }

    fn tools_for_request(&self) -> Vec<ToolDefinition> {
        match &self.child_role {
            Some(role) => self
                .tools
                .definitions()
                .into_iter()
                .filter(|tool| child_tool_name_allowed(&tool.name, Some(role), &[]))
                .collect(),
            None => self.tools.definitions(),
        }
    }

    fn child_tool_definitions(
        &self,
        role: Option<&str>,
        allowed_tools: &[String],
    ) -> Vec<ToolDefinition> {
        self.tools
            .definitions()
            .into_iter()
            .filter(|tool| child_tool_name_allowed(&tool.name, role, allowed_tools))
            .collect()
    }

    fn configured_agent(&self, name: &str) -> Option<AgentConfig> {
        self.configured_agents
            .iter()
            .find(|agent| agent.name == name)
            .cloned()
    }

    fn resolve_child_provider(
        &self,
        requested_preset: Option<ProviderPreset>,
        requested_model: Option<&str>,
    ) -> Result<(OpenAiClient, ProviderConfig), String> {
        let model = requested_model
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        let preset = match requested_preset {
            Some(preset) => preset,
            None => match &model {
                Some(model) => infer_child_provider(model, self.provider_config.preset),
                None => self.provider_config.preset,
            },
        };
        let (provider, mut provider_config) = if preset == self.provider_config.preset {
            (self.provider.clone(), self.provider_config.clone())
        } else {
            let resolver = self.child_provider_resolver.as_ref().ok_or_else(|| {
                format!(
                    "cross-provider child agents are not configured; use {} models only",
                    self.provider_config.preset.label()
                )
            })?;
            let mut provider_config = resolver(preset)?;
            provider_config
                .validate()
                .map_err(|error| format!("invalid child provider configuration: {error}"))?;
            provider_config.normalize_thinking();
            let api_key =
                secrets::api_key_cached_only(preset).map_err(|error| error.to_string())?;
            let provider = OpenAiClient::new(provider_config.base_url.clone(), api_key)
                .map_err(|error| error.to_string())?;
            (provider, provider_config)
        };
        if let Some(model) = model {
            provider_config.model = model;
        }
        validate_child_model(&provider_config)?;
        provider_config.normalize_thinking();
        provider_config.use_previous_response_id = false;
        Ok((provider, provider_config))
    }

    async fn compact_if_needed(
        &self,
        items: &mut Vec<ConversationItem>,
        ui_events: &mpsc::Sender<AgentEvent>,
    ) {
        if !self.compaction.enabled {
            return;
        }
        let Some(window) = self.provider_config.resolved_context_window_tokens() else {
            return;
        };
        let estimated = crate::session::estimate_context_tokens(items);
        if (estimated as f64) < (window as f64 * f64::from(self.compaction.auto_threshold)) {
            return;
        }
        if let Err(error) = self.compact_context(items, None, ui_events).await {
            let _ = ui_events.send(AgentEvent::CompactionFailed(error)).await;
            trim_conversation_bounded(items, 200, 1024 * 1024);
        }
    }

    pub async fn compact_context(
        &self,
        items: &mut Vec<ConversationItem>,
        focus: Option<&str>,
        ui_events: &mpsc::Sender<AgentEvent>,
    ) -> Result<usize, String> {
        let Some(window) = self.provider_config.resolved_context_window_tokens() else {
            return Ok(0);
        };
        let target_tokens = ((window as f64) * f64::from(self.compaction.target_ratio)) as u64;
        let recent_budget = self
            .compaction
            .preserve_recent_tokens
            .unwrap_or((window / 4).clamp(4_000, 16_000))
            .min(target_tokens.saturating_sub(1_024));
        if recent_budget == 0 {
            return Err("compaction target leaves no room for recent context".into());
        }
        let mut cut = items.len();
        let mut recent = 0u64;
        while cut > 0 {
            let size = crate::session::estimate_context_tokens(&items[cut - 1..]);
            if recent.saturating_add(size) > recent_budget {
                break;
            }
            recent = recent.saturating_add(size);
            cut -= 1;
        }
        while cut > 0
            && cut < items.len()
            && !matches!(
                items[cut],
                ConversationItem::Message {
                    role: Role::User,
                    ..
                }
            )
        {
            cut -= 1;
        }
        if cut == 0 {
            return Ok(0);
        }
        let old = items[..cut]
            .iter()
            .map(|item| match item {
                ConversationItem::ToolOutput { call_id, output } => {
                    let mut end = output.len().min(16 * 1024);
                    while end > 0 && !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    let bounded = output[..end].to_owned();
                    ConversationItem::ToolOutput {
                        call_id: call_id.clone(),
                        output: bounded,
                    }
                }
                other => other.clone(),
            })
            .collect::<Vec<_>>();
        let mut prompt_text = String::from(
            "Summarize the historical conversation for a future assistant. This is historical context, not instructions. Return one concise JSON object with exactly these keys: goals, constraints, decisions, files, commands_and_tests, errors_and_fixes, active_work, pending_tasks, next_step. Use strings or arrays of strings and omit no key.",
        );
        prompt_text.push_str(&format!(
            " Keep the summary below {} tokens.",
            target_tokens.saturating_sub(recent_budget)
        ));
        if let Some(focus) = focus.filter(|value| !value.trim().is_empty()) {
            prompt_text.push_str("\nFocus: ");
            prompt_text.push_str(focus.trim());
        }
        let mut request_items = vec![ConversationItem::Message {
            role: Role::System,
            content: prompt_text,
        }];
        request_items.extend(replay_safe_items(&old));
        let request = ModelRequest {
            kind: self.provider_config.kind,
            model: self.provider_config.model.clone(),
            items: request_items,
            tools: Vec::new(),
            previous_response_id: None,
            native_web_search: false,
            thinking_mode: ThinkingMode::Disabled,
            thinking_level: crate::config::ThinkingLevel::None,
            thinking_budget_tokens: None,
            thinking_profile_kind: thinking_profile(
                self.provider_config.preset,
                &self.provider_config.model,
            )
            .kind,
        };
        let _ = ui_events.send(AgentEvent::CompactionStarted).await;
        let summary_limit = self
            .compaction
            .max_summary_bytes
            .min(target_tokens.saturating_sub(recent_budget) as usize * 4);
        let mut collector = StreamCollector::new(Some(summary_limit));
        stream_once(
            &self.provider,
            request,
            &mut collector,
            128,
            ui_events,
            |_| Ok(Forwarded::Ignore),
        )
        .await
        .map_err(|failure| match failure {
            StreamFailure::Provider(error)
            | StreamFailure::Handler(error)
            | StreamFailure::Join(error) => error,
            StreamFailure::EndedWithoutCompletion => {
                "compaction stream ended without completion".into()
            }
        })?;
        let raw_summary = collector.assistant_text.trim();
        if raw_summary.is_empty() {
            return Err("compaction returned an empty summary".into());
        }
        let summary = serde_json::from_str::<Value>(raw_summary)
            .ok()
            .filter(Value::is_object)
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| raw_summary.to_owned());
        let hidden = self
            .storage
            .compact_with_summary(
                &self.session_id,
                &summary,
                items.len().saturating_sub(cut).max(1),
            )
            .map_err(|error| error.to_string())?;
        let mut canonical = vec![ConversationItem::CompactionSummary { content: summary }];
        canonical.extend_from_slice(&items[cut..]);
        *items = canonical;
        let _ = ui_events
            .send(AgentEvent::CompactionCompleted { hidden })
            .await;
        Ok(hidden)
    }

    pub async fn run(&self, mut items: Vec<ConversationItem>, ui_events: mpsc::Sender<AgentEvent>) {
        self.compact_if_needed(&mut items, &ui_events).await;
        if let Err(error) = self.run_at_depth(&mut items, &ui_events, 0).await {
            if error.starts_with("cancelled:") {
                let _ = ui_events.send(AgentEvent::Cancelled(error)).await;
            } else {
                let _ = ui_events.send(AgentEvent::Failed(error)).await;
            }
        }
    }

    async fn run_at_depth(
        &self,
        items: &mut Vec<ConversationItem>,
        ui_events: &mpsc::Sender<AgentEvent>,
        depth: usize,
    ) -> Result<(), String> {
        self.run_inner(items, ui_events, depth).await
    }

    async fn run_inner(
        &self,
        items: &mut Vec<ConversationItem>,
        ui_events: &mpsc::Sender<AgentEvent>,
        depth: usize,
    ) -> Result<(), String> {
        let mut previous_response_id = if self.provider_config.use_previous_response_id {
            self.storage
                .response_id(&self.session_id)
                .map_err(|error| error.to_string())?
        } else {
            None
        };
        // A persisted response already contains all but the newly appended user item.
        let mut request_cursor = if previous_response_id.is_some() {
            incremental_request_cursor(items)
        } else {
            0
        };
        let native_web_search = self.provider_config.preset == ProviderPreset::DeepSeek
            && self.provider_config.kind == ProviderKind::Responses
            && self.provider_config.native_web_search != NativeWebSearch::Disabled;
        let mut executed_tool_calls = HashSet::<String>::new();
        loop {
            let request_items = if previous_response_id.is_some() {
                items[request_cursor..].to_vec()
            } else {
                replay_safe_items(items)
            };
            let mut request_items = request_items;
            if previous_response_id.is_none() {
                request_items.insert(
                    0,
                    ConversationItem::Message {
                        role: Role::System,
                        content: match &self.child_role {
                            Some(role) => prompt::child_system_prompt(Some(role), &[]),
                            None => prompt::system_prompt(
                                self.provider_config.preset,
                                self.tools.mode(),
                            ),
                        },
                    },
                );
            }
            let request = ModelRequest {
                kind: self.provider_config.kind,
                model: self.provider_config.model.clone(),
                items: request_items,
                tools: self.tools_for_request(),
                previous_response_id: previous_response_id.clone(),
                native_web_search,
                thinking_mode: thinking_mode_for(&self.provider_config),
                thinking_level: self.provider_config.thinking_level,
                thinking_budget_tokens: self.provider_config.thinking_budget_tokens,
                thinking_profile_kind: thinking_profile(
                    self.provider_config.preset,
                    &self.provider_config.model,
                )
                .kind,
            };
            ui_events
                .send(AgentEvent::ModelStreaming)
                .await
                .map_err(|_| "UI event receiver closed".to_owned())?;
            let mut collector = StreamCollector::new(None);
            let mut search_results = 0usize;
            let mut search_bytes = 0usize;
            match stream_once(
                &self.provider,
                request,
                &mut collector,
                128,
                ui_events,
                |event| match event {
                    ModelEvent::WebSearchStarted { query } => {
                        Ok(Forwarded::Send(AgentEvent::WebSearchStarted { query }))
                    }
                    ModelEvent::WebSearchResult {
                        title,
                        url,
                        snippet,
                    } => {
                        let item_bytes = title.len() + url.len() + snippet.len();
                        if search_results < 10 && search_bytes + item_bytes <= 64 * 1024 {
                            search_results += 1;
                            search_bytes += item_bytes;
                            let label = format!("搜索来源：{title}");
                            let content = format!("{url}\n{snippet}");
                            items.push(ConversationItem::Context {
                                label: label.clone(),
                                content: content.clone(),
                            });
                            self.storage
                                .append_context(&self.session_id, &label, &content)
                                .map_err(|error| error.to_string())?;
                            Ok(Forwarded::Send(AgentEvent::WebSearchResult {
                                title,
                                url,
                                snippet,
                            }))
                        } else {
                            Ok(Forwarded::Ignore)
                        }
                    }
                    ModelEvent::WebSearchCompleted { count } => {
                        Ok(Forwarded::Send(AgentEvent::WebSearchCompleted {
                            count: if count == 0 {
                                search_results
                            } else {
                                count.min(10)
                            },
                        }))
                    }
                    ModelEvent::ProviderItem(item) => {
                        let encoded = serde_json::to_vec(&item)
                            .map_err(|error| format!("invalid provider item: {error}"))?;
                        if encoded.len() <= 64 * 1024 {
                            items.push(ConversationItem::ProviderItem { item: item.clone() });
                            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                                self.storage
                                    .append_provider_item(&self.session_id, &item)
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                        Ok(Forwarded::Ignore)
                    }
                    ModelEvent::ReasoningDelta(delta) => {
                        Ok(Forwarded::Send(AgentEvent::ReasoningDelta(delta)))
                    }
                    ModelEvent::TextDelta(delta) => {
                        Ok(Forwarded::Send(AgentEvent::TextDelta(delta)))
                    }
                    ModelEvent::Usage(usage) => Ok(Forwarded::SendIgnore(AgentEvent::Usage(usage))),
                    ModelEvent::ResponseId(id) => {
                        if self.provider_config.use_previous_response_id {
                            previous_response_id = Some(id.clone());
                            self.storage
                                .save_response_id(&self.session_id, &id)
                                .map_err(|error| error.to_string())?;
                        }
                        Ok(Forwarded::Ignore)
                    }
                    ModelEvent::ToolCallDelta { .. }
                    | ModelEvent::ToolCallComplete(_)
                    | ModelEvent::Done => Ok(Forwarded::Ignore),
                },
            )
            .await
            {
                Ok(()) => {}
                Err(StreamFailure::Provider(error)) => {
                    if previous_response_id.is_some()
                        && collector.assistant_text.is_empty()
                        && collector.partials.is_empty()
                        && collector.completed_calls.is_empty()
                    {
                        // Compatible endpoints can expire or reject server-side state.
                        // Replay the canonical local history once instead.
                        self.storage
                            .clear_response_id(&self.session_id)
                            .map_err(|error| error.to_string())?;
                        previous_response_id = None;
                        request_cursor = 0;
                        continue;
                    }
                    return Err(error);
                }
                Err(StreamFailure::Handler(error)) | Err(StreamFailure::Join(error)) => {
                    return Err(error);
                }
                Err(StreamFailure::EndedWithoutCompletion) => {
                    return Err("model stream ended without completion".into());
                }
            }
            collector.finish_partials("tool")?;
            let reasoning_text = collector.reasoning_text.trim();
            if !reasoning_text.is_empty() {
                items.push(ConversationItem::ThinkingSummary {
                    content: reasoning_text.to_owned(),
                });
                self.storage
                    .append_thinking_summary(&self.session_id, reasoning_text)
                    .map_err(|error| error.to_string())?;
            }
            if !collector.assistant_text.is_empty() {
                items.push(ConversationItem::Message {
                    role: Role::Assistant,
                    content: collector.assistant_text.clone(),
                });
                self.storage
                    .append_message(&self.session_id, Role::Assistant, &collector.assistant_text)
                    .map_err(|error| error.to_string())?;
            }
            if collector.completed_calls.is_empty() {
                ui_events
                    .send(AgentEvent::Completed {
                        items: items.clone(),
                    })
                    .await
                    .map_err(|_| "UI event receiver closed".to_owned())?;
                return Ok(());
            }

            items.push(ConversationItem::AssistantToolCalls {
                calls: collector.completed_calls.clone(),
            });
            self.storage
                .append_tool_calls(&self.session_id, &collector.completed_calls)
                .map_err(|error| error.to_string())?;
            // When Responses server state is enabled, the response already owns the
            // assistant text and tool calls. Only subsequent tool outputs are new.
            request_cursor = items.len();
            let mut spawn_tasks: Vec<ToolCall> = Vec::new();
            for call in collector.completed_calls {
                let signature = tool_call_signature(&call);
                if executed_tool_calls.contains(&signature) {
                    let result = "Duplicate tool call was not executed. Reuse the previous result or choose a different action.".to_owned();
                    ui_events
                        .send(AgentEvent::ToolFinished {
                            call: call.clone(),
                            result: result.clone(),
                        })
                        .await
                        .map_err(|_| "UI event receiver closed".to_owned())?;
                    items.push(ConversationItem::ToolOutput {
                        call_id: call.id.clone(),
                        output: result.clone(),
                    });
                    self.storage
                        .append_tool_output(&self.session_id, &call.id, &result)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                if let Some(role) = &self.child_role
                    && !child_tool_name_allowed(&call.name, Some(role), &[])
                {
                    let result =
                        format!("denied by policy: child role does not allow {}", call.name);
                    self.storage
                        .begin_tool(&self.session_id, &call, "denied")
                        .map_err(|error| error.to_string())?;
                    self.complete_tool(&call, &result, ui_events, items, &mut executed_tool_calls)
                        .await?;
                    continue;
                }
                let decision = self.tools.policy(&call);
                let approved = match decision {
                    PolicyDecision::Allow => true,
                    PolicyDecision::Deny(reason) => {
                        self.storage
                            .begin_tool(&self.session_id, &call, "denied")
                            .map_err(|error| error.to_string())?;
                        let result = format!("denied by policy: {reason}");
                        self.complete_tool(
                            &call,
                            &result,
                            ui_events,
                            items,
                            &mut executed_tool_calls,
                        )
                        .await?;
                        continue;
                    }
                    PolicyDecision::RequireApproval(reason) => {
                        let (reply, answer) = oneshot::channel();
                        ui_events
                            .send(AgentEvent::Approval {
                                call: call.clone(),
                                reason,
                                source_session_id: None,
                                source_title: None,
                                reply,
                            })
                            .await
                            .map_err(|_| "UI event receiver closed".to_owned())?;
                        answer
                            .await
                            .map_err(|_| "cancelled: approval channel closed".to_owned())?
                    }
                };
                let decision_name = if approved { "approved" } else { "rejected" };
                self.storage
                    .begin_tool(&self.session_id, &call, decision_name)
                    .map_err(|error| error.to_string())?;
                if !approved {
                    self.complete_tool(
                        &call,
                        "rejected by user",
                        ui_events,
                        items,
                        &mut executed_tool_calls,
                    )
                    .await?;
                    continue;
                }
                ui_events
                    .send(AgentEvent::ToolStarted(call.clone()))
                    .await
                    .map_err(|_| "UI event receiver closed".to_owned())?;
                if call.name == "agent_spawn" {
                    if depth >= 1 {
                        self.complete_tool(
                            &call,
                            "child agents cannot recursively spawn another child",
                            ui_events,
                            items,
                            &mut executed_tool_calls,
                        )
                        .await?;
                    } else {
                        spawn_tasks.push(call);
                    }
                } else {
                    let result = self
                        .tools
                        .execute(&call)
                        .await
                        .unwrap_or_else(|error| error.to_string());
                    self.complete_tool(&call, &result, ui_events, items, &mut executed_tool_calls)
                        .await?;
                }
            }

            if !spawn_tasks.is_empty() {
                let mut futures = FuturesUnordered::new();
                for call in spawn_tasks {
                    let runner = self.clone();
                    let ui_events = ui_events.clone();
                    futures.push(async move {
                        let result = runner
                            .run_child(&call, &ui_events)
                            .await
                            .unwrap_or_else(|error| error);
                        (call, result)
                    });
                }
                while let Some((call, result)) = futures.next().await {
                    self.complete_tool(&call, &result, ui_events, items, &mut executed_tool_calls)
                        .await?;
                }
            }
        }
    }

    /// Finishes an already-started tool call: persists the result, emits the
    /// `ToolFinished` event, and appends the tool output to the conversation.
    async fn complete_tool(
        &self,
        call: &ToolCall,
        result: &str,
        ui_events: &mpsc::Sender<AgentEvent>,
        items: &mut Vec<ConversationItem>,
        executed_tool_calls: &mut HashSet<String>,
    ) -> Result<(), String> {
        self.storage
            .finish_tool(&call.id, result)
            .map_err(|error| error.to_string())?;
        ui_events
            .send(AgentEvent::ToolFinished {
                call: call.clone(),
                result: result.to_owned(),
            })
            .await
            .map_err(|_| "UI event receiver closed".to_owned())?;
        items.push(ConversationItem::ToolOutput {
            call_id: call.id.clone(),
            output: result.to_owned(),
        });
        self.storage
            .append_tool_output(&self.session_id, &call.id, result)
            .map_err(|error| error.to_string())?;
        executed_tool_calls.insert(tool_call_signature(call));
        Ok(())
    }

    /// Executes one child-agent tool call, honouring the shared policy. Write
    /// tools request approval (serialized through `approval_lock` so concurrent
    /// children cannot interleave approval prompts).
    async fn execute_child_tool(
        &self,
        context: &ChildToolContext<'_>,
        call: &ToolCall,
        active_budget: &mut Duration,
    ) -> Option<String> {
        match self.tools.policy(call) {
            PolicyDecision::Allow => {
                let _ = self.storage.begin_tool(context.child_id, call, "allowed");
                emit_child_progress(
                    context.ui_events,
                    context.child_id,
                    child_progress(
                        ChildSessionStatus::RunningTool,
                        context.turn,
                        context.max_turns,
                        Some(call.name.clone()),
                    ),
                )
                .await;
                let result = self
                    .execute_child_tool_with_budget(call, active_budget)
                    .await?;
                let _ = self.storage.finish_tool(&call.id, &result);
                Some(result)
            }
            PolicyDecision::Deny(reason) => {
                let result = format!("denied by policy: {reason}");
                let _ = self.storage.begin_tool(context.child_id, call, "denied");
                let _ = self.storage.finish_tool(&call.id, &result);
                Some(result)
            }
            PolicyDecision::RequireApproval(reason) => {
                emit_child_progress(
                    context.ui_events,
                    context.child_id,
                    child_progress(
                        ChildSessionStatus::WaitingApprovalSlot,
                        context.turn,
                        context.max_turns,
                        Some(call.name.clone()),
                    ),
                )
                .await;
                let _guard = self.approval_lock.lock().await;
                emit_child_progress(
                    context.ui_events,
                    context.child_id,
                    child_progress(
                        ChildSessionStatus::WaitingApproval,
                        context.turn,
                        context.max_turns,
                        Some(call.name.clone()),
                    ),
                )
                .await;
                let (reply, answer) = oneshot::channel();
                if context
                    .ui_events
                    .send(AgentEvent::Approval {
                        call: call.clone(),
                        reason,
                        source_session_id: Some(context.child_id.to_owned()),
                        source_title: Some(context.child_title.to_owned()),
                        reply,
                    })
                    .await
                    .is_err()
                {
                    return Some("approval channel closed".into());
                }
                let approved = match answer.await {
                    Ok(approved) => approved,
                    Err(_) => return Some("approval cancelled".into()),
                };
                let _ = self.storage.begin_tool(
                    context.child_id,
                    call,
                    if approved { "approved" } else { "rejected" },
                );
                let result = if approved {
                    emit_child_progress(
                        context.ui_events,
                        context.child_id,
                        child_progress(
                            ChildSessionStatus::RunningTool,
                            context.turn,
                            context.max_turns,
                            Some(call.name.clone()),
                        ),
                    )
                    .await;
                    self.execute_child_tool_with_budget(call, active_budget)
                        .await?
                } else {
                    "rejected by user".to_owned()
                };
                let _ = self.storage.finish_tool(&call.id, &result);
                Some(result)
            }
        }
    }

    async fn execute_child_tool_with_budget(
        &self,
        call: &ToolCall,
        active_budget: &mut Duration,
    ) -> Option<String> {
        if active_budget.is_zero() {
            return None;
        }
        let started = Instant::now();
        let result = timeout(*active_budget, self.tools.execute(call)).await;
        *active_budget = active_budget.saturating_sub(started.elapsed());
        match result {
            Ok(result) => Some(result.unwrap_or_else(|error| error.to_string())),
            Err(_) => None,
        }
    }

    async fn run_child(
        &self,
        call: &ToolCall,
        ui_events: &mpsc::Sender<AgentEvent>,
    ) -> Result<String, String> {
        let arguments: ChildArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| format!("invalid child agent arguments: {error}"))?;
        if arguments.prompt.trim().is_empty() {
            return Err("child agent prompt must not be empty".into());
        }
        let configured_agent = match arguments.agent.as_deref().map(str::trim) {
            Some("") | None => None,
            Some(name) => match self.configured_agent(name) {
                Some(agent) => Some(agent),
                None => return Err(format!("unknown configured agent \"{name}\"")),
            },
        };
        let role = arguments
            .role
            .clone()
            .or_else(|| configured_agent.as_ref().map(|agent| agent.name.clone()));
        let allowed_tools = configured_agent
            .as_ref()
            .map(|agent| agent.allowed_tools.clone())
            .unwrap_or_default();
        let role = match role {
            Some(role) => Some(role),
            None if allowed_tools.iter().any(|tool| {
                tool == "file_write"
                    || tool == "file_mkdir"
                    || tool == "file_copy"
                    || tool == "file_move"
            }) =>
            {
                Some("implement".to_owned())
            }
            None => None,
        };
        let max_turns = arguments
            .max_turns
            .or_else(|| configured_agent.as_ref().map(|agent| agent.max_turns))
            .unwrap_or(0);

        let requested_preset = match arguments.provider.as_deref().map(str::trim) {
            Some("") | None => None,
            Some(name) => Some(
                ProviderPreset::parse(name)
                    .ok_or_else(|| format!("unknown provider preset \"{name}\" for agent_spawn"))?,
            ),
        };
        let (provider, provider_config) =
            self.resolve_child_provider(requested_preset, arguments.model.as_deref())?;

        // Create a nested session so the child's work is inspectable from the
        // session panel, using its own provider/model when one was requested.
        let workspace = self
            .storage
            .session_workspace(&self.session_id)
            .map_err(|error| error.to_string())?;
        let title = child_title(&arguments, configured_agent.as_ref(), role.as_deref());
        let child_mode = if is_implement_role(role.as_deref()) {
            "build"
        } else {
            "explore"
        };
        let child_role = role.as_deref().unwrap_or("");
        let child_id = self
            .storage
            .create_child_session(
                Path::new(&workspace),
                &self.session_id,
                provider_config.preset.key_id(),
                &provider_config.model,
                &title,
                child_mode,
                child_role,
            )
            .map_err(|error| error.to_string())?;
        self.storage
            .append_message(&child_id, Role::User, &arguments.prompt)
            .map_err(|error| error.to_string())?;
        let mut cancellation_guard = ChildCancellationGuard {
            ui_events: ui_events.clone(),
            session_id: child_id.clone(),
            max_turns,
            finished: false,
        };
        // Let the UI refresh the session tree as soon as the child exists,
        // rather than waiting for the whole turn to complete.
        let _ = ui_events.send(AgentEvent::SessionsChanged).await;
        emit_child_progress(
            ui_events,
            &child_id,
            child_progress(ChildSessionStatus::Queued, 0, max_turns, None),
        )
        .await;
        let _child_slot = self
            .child_slots
            .acquire()
            .await
            .map_err(|_| "child concurrency limiter closed".to_owned())?;

        let tools = self.child_tool_definitions(role.as_deref(), &allowed_tools);
        let mut child_system = prompt::child_system_prompt(role.as_deref(), &allowed_tools);
        if let Some(agent) = &configured_agent
            && !agent.system_prompt.trim().is_empty()
        {
            child_system.push_str("\n\nADDITIONAL AGENT INSTRUCTIONS\n");
            child_system.push_str(agent.system_prompt.trim());
        }

        // Multi-turn loop: execute the child's role-filtered tools, keep its
        // context bounded, and return only the final deliverable.
        let thinking_profile_kind =
            thinking_profile(provider_config.preset, &provider_config.model).kind;
        let native_web_search = provider_config.preset == ProviderPreset::DeepSeek
            && provider_config.kind == ProviderKind::Responses
            && provider_config.native_web_search != NativeWebSearch::Disabled;
        let child_max_output_bytes = self.cluster.child_max_output_bytes;

        let mut items = vec![ConversationItem::Message {
            role: Role::User,
            content: arguments.prompt.clone(),
        }];
        let mut final_answer = String::new();
        let mut tool_call_count = 0usize;
        let mut remaining_turns = max_turns;
        let mut completed_turns = 0usize;
        let mut active_budget =
            Duration::from_secs(self.cluster.child_active_timeout_seconds.max(1));
        let mut failure: Option<String> = None;
        let mut status = ChildSessionStatus::Completed;
        'turns: loop {
            if max_turns > 0 && remaining_turns == 0 {
                status = ChildSessionStatus::TurnLimit;
                let trail = summarize_child_trail(&items, 3, 512);
                append_text_bounded(
                    &mut final_answer,
                    &format!("\n[child agent reached its turn limit]{trail}"),
                    child_max_output_bytes,
                );
                break;
            }
            if max_turns > 0 {
                remaining_turns -= 1;
            }
            completed_turns = completed_turns.saturating_add(1);
            let turn = completed_turns;

            let mut request_items = vec![ConversationItem::Message {
                role: Role::System,
                content: child_system.clone(),
            }];
            request_items.extend(items.clone());
            let request = ModelRequest {
                kind: provider_config.kind,
                model: provider_config.model.clone(),
                items: request_items,
                tools: tools.clone(),
                previous_response_id: None,
                native_web_search,
                thinking_mode: thinking_mode_for(&provider_config),
                thinking_level: provider_config.thinking_level,
                thinking_budget_tokens: provider_config.thinking_budget_tokens,
                thinking_profile_kind,
            };
            let mut collector = StreamCollector::new(Some(child_max_output_bytes));
            emit_child_progress(
                ui_events,
                &child_id,
                child_progress(ChildSessionStatus::WaitingModel, turn, max_turns, None),
            )
            .await;
            if active_budget.is_zero() {
                status = ChildSessionStatus::TimedOut;
                break;
            }
            let stream_started = Instant::now();
            let mut streaming_reported = false;
            let stream_result = timeout(
                active_budget,
                stream_once(&provider, request, &mut collector, 512, ui_events, |_| {
                    if streaming_reported {
                        Ok(Forwarded::Ignore)
                    } else {
                        streaming_reported = true;
                        Ok(Forwarded::SendIgnore(AgentEvent::ChildSessionProgress {
                            session_id: child_id.clone(),
                            progress: child_progress(
                                ChildSessionStatus::Streaming,
                                turn,
                                max_turns,
                                None,
                            ),
                        }))
                    }
                }),
            )
            .await;
            active_budget = active_budget.saturating_sub(stream_started.elapsed());
            let stream_result = match stream_result {
                Ok(result) => result,
                Err(_) => {
                    status = ChildSessionStatus::TimedOut;
                    if !collector.assistant_text.is_empty() {
                        append_text_bounded(
                            &mut final_answer,
                            &collector.assistant_text,
                            child_max_output_bytes,
                        );
                    }
                    break;
                }
            };
            match stream_result {
                Ok(()) => {}
                Err(StreamFailure::Provider(error)) => {
                    status = ChildSessionStatus::Failed;
                    failure = Some(error);
                    break;
                }
                Err(StreamFailure::Handler(error)) | Err(StreamFailure::Join(error)) => {
                    return Err(error);
                }
                Err(StreamFailure::EndedWithoutCompletion) => {
                    status = ChildSessionStatus::Failed;
                    failure = Some("child stream ended without completion".into());
                    break;
                }
            }
            collector.finish_partials("child tool")?;
            if collector.completed_calls.is_empty() {
                final_answer = collector.assistant_text.clone();
                break;
            }

            if !collector.assistant_text.is_empty() {
                items.push(ConversationItem::Message {
                    role: Role::Assistant,
                    content: std::mem::take(&mut collector.assistant_text),
                });
            }
            items.push(ConversationItem::AssistantToolCalls {
                calls: collector.completed_calls.clone(),
            });
            self.storage
                .append_tool_calls(&child_id, &collector.completed_calls)
                .map_err(|error| error.to_string())?;
            for tool_call in std::mem::take(&mut collector.completed_calls) {
                tool_call_count += 1;
                let context = ChildToolContext {
                    child_id: &child_id,
                    child_title: &title,
                    ui_events,
                    turn,
                    max_turns,
                };
                let Some(result) = self
                    .execute_child_tool(&context, &tool_call, &mut active_budget)
                    .await
                else {
                    let result = "child active execution budget exceeded".to_owned();
                    let _ = self.storage.finish_tool(&tool_call.id, &result);
                    let _ = self
                        .storage
                        .append_tool_output(&child_id, &tool_call.id, &result);
                    items.push(ConversationItem::ToolOutput {
                        call_id: tool_call.id,
                        output: result,
                    });
                    status = ChildSessionStatus::TimedOut;
                    break 'turns;
                };
                let result =
                    truncate_utf8_bounded(&result, self.cluster.child_max_tool_output_bytes);
                self.storage
                    .append_tool_output(&child_id, &tool_call.id, &result)
                    .map_err(|error| error.to_string())?;
                items.push(ConversationItem::ToolOutput {
                    call_id: tool_call.id.clone(),
                    output: result,
                });
                trim_conversation_bounded(
                    &mut items,
                    self.cluster.child_max_context_items,
                    self.cluster.child_max_context_bytes,
                );
            }
        }

        if let Some(error) = failure {
            append_text_bounded(
                &mut final_answer,
                &format!("\n[child failed: {error}]"),
                child_max_output_bytes,
            );
        }
        if status == ChildSessionStatus::TimedOut {
            let trail = summarize_child_trail(&items, 3, 512);
            append_text_bounded(
                &mut final_answer,
                &format!("\n[child agent exceeded its active execution budget]{trail}"),
                child_max_output_bytes,
            );
        }
        if final_answer.trim().is_empty() {
            if tool_call_count > 0 {
                final_answer.push_str(&format!(
                    "[child agent issued {tool_call_count} tool call(s) but returned no text]"
                ));
            } else {
                final_answer.push_str("[child agent returned no text]");
            }
        }
        let final_answer = truncate_utf8_bounded(&final_answer, child_max_output_bytes);
        self.storage
            .append_message(&child_id, Role::Assistant, &final_answer)
            .map_err(|error| error.to_string())?;
        emit_child_progress(
            ui_events,
            &child_id,
            child_progress(status, completed_turns, max_turns, None),
        )
        .await;
        cancellation_guard.finish();
        Ok(serde_json::to_string(&json!({
            "session_id": child_id,
            "title": title,
            "status": status.wire_name(),
            "output": final_answer,
        }))
        .unwrap_or_else(|_| final_answer.clone()))
    }
}

fn tool_call_signature(call: &ToolCall) -> String {
    format!("{}:{}", call.name, canonical_json(&call.arguments))
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let mut fields = object.iter().collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(key, _)| *key);
            let fields = fields
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("JSON object keys are serializable"),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{fields}}}")
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => serde_json::to_string(value).expect("JSON values are serializable"),
    }
}

fn qwen_thinking_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("qwen-plus")
        || model.contains("qwen-max")
        || model.contains("qwen-turbo")
        || model.contains("qwen3")
        || model.contains("qwq")
        || model.contains("qwen-flash")
}

fn volcano_thinking_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("thinking") || model.contains("reason") || model.contains("seed")
}

fn thinking_mode_for(config: &ProviderConfig) -> ThinkingMode {
    let capability = match config.thinking {
        ThinkingCapability::Auto => None,
        ThinkingCapability::OpenAi => Some(if config.kind == ProviderKind::Responses {
            ThinkingMode::OpenAiResponsesSummary
        } else {
            ThinkingMode::CompatibleAuto
        }),
        ThinkingCapability::DeepSeek => Some(match config.kind {
            ProviderKind::Responses => ThinkingMode::DeepSeekResponses,
            ProviderKind::ChatCompletions => ThinkingMode::DeepSeekChat,
        }),
        ThinkingCapability::Qwen => Some(if config.kind == ProviderKind::ChatCompletions {
            ThinkingMode::QwenChat
        } else {
            ThinkingMode::QwenResponses
        }),
        ThinkingCapability::Volcano => Some(if config.kind == ProviderKind::ChatCompletions {
            ThinkingMode::VolcanoChat
        } else {
            ThinkingMode::CompatibleAuto
        }),
        ThinkingCapability::Compatible => Some(ThinkingMode::CompatibleAuto),
        ThinkingCapability::Disabled => Some(ThinkingMode::Disabled),
    };
    if let Some(mode) = capability {
        return mode;
    }
    match (config.preset, config.kind) {
        (ProviderPreset::OpenAi, ProviderKind::Responses) => ThinkingMode::OpenAiResponsesSummary,
        (ProviderPreset::DeepSeek, ProviderKind::Responses) => ThinkingMode::DeepSeekResponses,
        (ProviderPreset::DeepSeek, ProviderKind::ChatCompletions) => ThinkingMode::DeepSeekChat,
        (ProviderPreset::Qwen, ProviderKind::Responses) if qwen_thinking_model(&config.model) => {
            ThinkingMode::QwenResponses
        }
        (ProviderPreset::Qwen, ProviderKind::ChatCompletions)
            if qwen_thinking_model(&config.model) =>
        {
            ThinkingMode::QwenChat
        }
        (ProviderPreset::Volcano, ProviderKind::ChatCompletions)
            if volcano_thinking_model(&config.model) =>
        {
            ThinkingMode::VolcanoChat
        }
        (ProviderPreset::Custom, ProviderKind::ChatCompletions) => ThinkingMode::CompatibleAuto,
        _ => ThinkingMode::Disabled,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildArgs {
    prompt: String,
    max_turns: Option<usize>,
    role: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    agent: Option<String>,
    title: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::{config::RuntimeConfig, security::Workspace, tools::ToolRegistry};

    #[test]
    fn child_status_has_stable_wire_names_and_localized_labels() {
        let terminal = [
            (ChildSessionStatus::Completed, "completed", "完成"),
            (ChildSessionStatus::Failed, "failed", "失败"),
            (ChildSessionStatus::TurnLimit, "turn_limit", "达到轮次上限"),
            (ChildSessionStatus::TimedOut, "timed_out", "执行超时"),
            (ChildSessionStatus::Cancelled, "cancelled", "已取消"),
        ];
        for (status, wire_name, label) in terminal {
            assert!(status.is_terminal());
            assert_eq!(status.wire_name(), wire_name);
            assert_eq!(status.label(), label);
        }
        assert_eq!(ChildSessionStatus::Queued.wire_name(), "running");
        assert_eq!(ChildSessionStatus::WaitingApproval.label(), "等待审批");
    }

    #[test]
    fn enables_thinking_only_for_known_qwen_thinking_families() {
        assert!(qwen_thinking_model("qwen3.8-max"));
        assert!(qwen_thinking_model("QWQ-32B"));
        assert!(qwen_thinking_model("qwen-plus"));
        assert!(qwen_thinking_model("qwen-max"));
        assert!(qwen_thinking_model("qwen-turbo"));
        assert!(!qwen_thinking_model("qwen2.5-coder"));
        assert!(!qwen_thinking_model("custom-model"));
    }

    #[test]
    fn selects_provider_specific_thinking_modes() {
        let mut config = ProviderPreset::OpenAi.defaults();
        assert_eq!(
            thinking_mode_for(&config),
            ThinkingMode::OpenAiResponsesSummary
        );

        config = ProviderPreset::DeepSeek.defaults();
        assert_eq!(thinking_mode_for(&config), ThinkingMode::DeepSeekResponses);
        config.kind = ProviderKind::ChatCompletions;
        assert_eq!(thinking_mode_for(&config), ThinkingMode::DeepSeekChat);

        config = ProviderPreset::Qwen.defaults();
        for model in [
            "qwen-plus",
            "qwen-max",
            "qwen-turbo",
            "qwen3-max",
            "qwq-32b",
        ] {
            config.model = model.into();
            assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenChat);
        }
        config.model = "unknown-qwen-model".into();
        assert_eq!(thinking_mode_for(&config), ThinkingMode::Disabled);
        config.model = "qwen3.8-max".into();
        config.kind = ProviderKind::Responses;
        assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenResponses);

        config = ProviderPreset::Volcano.defaults();
        assert_eq!(thinking_mode_for(&config), ThinkingMode::VolcanoChat);

        config = ProviderPreset::Custom.defaults();
        assert_eq!(thinking_mode_for(&config), ThinkingMode::CompatibleAuto);
        config.thinking = ThinkingCapability::Qwen;
        assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenChat);
        config.kind = ProviderKind::Responses;
        assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenResponses);
        config.thinking = ThinkingCapability::OpenAi;
        assert_eq!(
            thinking_mode_for(&config),
            ThinkingMode::OpenAiResponsesSummary
        );
        config.thinking = ThinkingCapability::Disabled;
        assert_eq!(thinking_mode_for(&config), ThinkingMode::Disabled);
    }

    #[test]
    fn tool_signatures_normalize_object_keys_but_preserve_values() {
        let first = ToolCall {
            id: "call-1".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"src/lib.rs","options":{"end":20,"start":1}}),
        };
        let reordered = ToolCall {
            id: "call-2".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"options":{"start":1,"end":20},"path":"src/lib.rs"}),
        };
        let different = ToolCall {
            id: "call-3".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"src/lib.rs","options":{"start":2,"end":20}}),
        };

        assert_eq!(tool_call_signature(&first), tool_call_signature(&reordered));
        assert_ne!(tool_call_signature(&first), tool_call_signature(&different));
    }

    #[test]
    fn incremental_cursor_keeps_latest_user_message_and_following_context() {
        let items = vec![
            ConversationItem::Message {
                role: Role::User,
                content: "old".into(),
            },
            ConversationItem::Message {
                role: Role::Assistant,
                content: "answer".into(),
            },
            ConversationItem::Message {
                role: Role::User,
                content: "new".into(),
            },
            ConversationItem::Context {
                label: "file".into(),
                content: "contents".into(),
            },
        ];
        assert_eq!(incremental_request_cursor(&items), 2);
        assert_eq!(items[incremental_request_cursor(&items)..].len(), 2);
    }

    #[test]
    fn stateless_replay_keeps_only_complete_ordered_tool_pairs() {
        let complete = ToolCall {
            id: "complete".into(),
            name: "agent_spawn".into(),
            arguments: json!({}),
        };
        let unanswered = ToolCall {
            id: "unanswered".into(),
            name: "agent_spawn".into(),
            arguments: json!({}),
        };
        let items = vec![
            ConversationItem::ToolOutput {
                call_id: "orphan".into(),
                output: "bad".into(),
            },
            ConversationItem::AssistantToolCalls {
                calls: vec![complete.clone(), unanswered],
            },
            ConversationItem::ToolOutput {
                call_id: complete.id.clone(),
                output: "ok".into(),
            },
            ConversationItem::Message {
                role: Role::User,
                content: "continue".into(),
            },
        ];

        let replay = replay_safe_items(&items);
        assert!(matches!(
            &replay[0],
            ConversationItem::AssistantToolCalls { calls }
                if calls.len() == 1 && calls[0].id == "complete"
        ));
        assert!(matches!(
            &replay[1],
            ConversationItem::ToolOutput { call_id, .. } if call_id == "complete"
        ));
        assert_eq!(replay.len(), 3);
    }

    #[tokio::test]
    async fn main_agent_completes_after_one_hundred_tool_rounds() {
        let mut responses = (0..100)
            .map(|round| {
                vec![
                    ModelEvent::ToolCallComplete(ToolCall {
                        id: format!("call-{round}"),
                        name: "file_read".into(),
                        arguments: serde_json::json!({"path":format!("missing-{round}")}),
                    }),
                    ModelEvent::Done,
                ]
            })
            .collect::<Vec<_>>();
        responses.push(vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done]);

        let temp = TempDir::new().unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        storage
            .append_message(&session_id, Role::User, "run tools")
            .unwrap();
        let mut provider_config = ProviderPreset::Custom.defaults();
        provider_config.model = "fixture".into();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let runner = AgentRunner::new(
            OpenAiClient::scripted(responses).unwrap(),
            provider_config,
            tools,
            storage,
            session_id,
        );
        let (events, mut receiver) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            runner
                .run(
                    vec![ConversationItem::Message {
                        role: Role::User,
                        content: "run tools".into(),
                    }],
                    events,
                )
                .await;
        });

        let mut completed = false;
        let mut failed = None;
        while let Some(event) = receiver.recv().await {
            match event {
                AgentEvent::Completed { .. } => completed = true,
                AgentEvent::Failed(error) => failed = Some(error),
                _ => {}
            }
        }
        task.await.unwrap();
        assert!(completed);
        assert!(failed.is_none(), "unexpected failure: {failed:?}");
    }

    #[tokio::test]
    async fn reasoning_deltas_are_persisted_as_thinking_summary() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        storage
            .append_message(&session_id, Role::User, "think")
            .unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let provider = OpenAiClient::scripted(vec![vec![
            ModelEvent::ReasoningDelta("第一段".into()),
            ModelEvent::ReasoningDelta("第二段".into()),
            ModelEvent::TextDelta("answer".into()),
            ModelEvent::Done,
        ]])
        .unwrap();
        let runner = AgentRunner::new(
            provider,
            ProviderPreset::Custom.defaults(),
            tools,
            storage.clone(),
            session_id.clone(),
        );
        let (events, mut receiver) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            runner
                .run(
                    vec![ConversationItem::Message {
                        role: Role::User,
                        content: "think".into(),
                    }],
                    events,
                )
                .await;
        });

        let mut completed = false;
        let mut reasoning_seen = false;
        while let Some(event) = receiver.recv().await {
            match event {
                AgentEvent::Completed { items } => {
                    completed = true;
                    assert!(items.iter().any(|item| {
                        matches!(
                            item,
                            ConversationItem::ThinkingSummary { content }
                                if content == "第一段第二段"
                        )
                    }));
                }
                AgentEvent::ReasoningDelta(_) => reasoning_seen = true,
                AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
                _ => {}
            }
        }
        task.await.unwrap();
        assert!(completed);
        assert!(reasoning_seen);

        let loaded = storage.load_messages(&session_id).unwrap();
        assert!(loaded.iter().any(|item| {
            matches!(
                item,
                ConversationItem::ThinkingSummary { content } if content == "第一段第二段"
            )
        }));
    }

    #[tokio::test]
    async fn child_agent_creates_nested_session_with_model_and_result() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        storage
            .append_message(&session_id, Role::User, "parent")
            .unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let provider = OpenAiClient::scripted(vec![vec![
            ModelEvent::TextDelta("child result".into()),
            ModelEvent::Done,
        ]])
        .unwrap();
        let runner = AgentRunner::new(
            provider,
            ProviderPreset::OpenAi.defaults(),
            tools,
            storage.clone(),
            session_id.clone(),
        );

        let call = ToolCall {
            id: "call-1".into(),
            name: "agent_spawn".into(),
            arguments: serde_json::json!({"prompt":"do the plan","role":"plan","model":"gpt-5"}),
        };
        let (ui_events, mut receiver) = mpsc::channel(16);
        let result = runner.run_child(&call, &ui_events).await.unwrap();
        let payload: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["output"], "child result");
        assert_eq!(payload["title"], "plan·do the plan");

        let sessions = storage.list_sessions(temp.path()).unwrap();
        assert_eq!(sessions.len(), 2);
        let child = sessions.iter().find(|s| s.id != session_id).unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(child.title, "plan·do the plan");
        assert_eq!(
            storage.session_provider_model(&child.id).unwrap().1,
            "gpt-5"
        );
        assert_eq!(storage.load_messages(&child.id).unwrap().len(), 2);

        let mut statuses = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let AgentEvent::ChildSessionProgress { progress, .. } = event {
                statuses.push(progress.status);
            }
        }
        assert_eq!(
            statuses,
            vec![
                ChildSessionStatus::Queued,
                ChildSessionStatus::WaitingModel,
                ChildSessionStatus::Streaming,
                ChildSessionStatus::Completed,
            ]
        );
    }

    #[tokio::test]
    async fn child_concurrency_slots_enforce_the_configured_limit() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let runner = AgentRunner::new(
            OpenAiClient::scripted(Vec::new()).unwrap(),
            ProviderPreset::OpenAi.defaults(),
            tools,
            storage,
            session_id,
        )
        .with_cluster_config(ClusterConfig {
            max_parallel_children: Some(2),
            ..ClusterConfig::default()
        });

        let first = runner.child_slots.try_acquire().unwrap();
        let _second = runner.child_slots.try_acquire().unwrap();
        assert!(runner.child_slots.try_acquire().is_err());
        drop(first);
        assert!(runner.child_slots.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn zero_child_tool_budget_does_not_execute_the_tool() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let runner = AgentRunner::new(
            OpenAiClient::scripted(Vec::new()).unwrap(),
            ProviderPreset::OpenAi.defaults(),
            tools,
            storage,
            session_id,
        );
        let call = ToolCall {
            id: "must-not-run".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"missing"}),
        };
        let mut budget = Duration::ZERO;
        assert!(
            runner
                .execute_child_tool_with_budget(&call, &mut budget)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancellation_progress_survives_a_temporarily_full_channel() {
        let (events, mut receiver) = mpsc::channel(1);
        events.send(AgentEvent::SessionsChanged).await.unwrap();
        let guard = ChildCancellationGuard {
            ui_events: events,
            session_id: "cancelled-child".into(),
            max_turns: 3,
            finished: false,
        };
        drop(guard);

        assert!(matches!(
            receiver.recv().await,
            Some(AgentEvent::SessionsChanged)
        ));
        let cancelled = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            cancelled,
            AgentEvent::ChildSessionProgress {
                session_id,
                progress: ChildSessionProgress {
                    status: ChildSessionStatus::Cancelled,
                    ..
                },
            } if session_id == "cancelled-child"
        ));
    }

    #[test]
    fn child_tool_filter_never_grants_terminal_or_spawn() {
        assert!(child_tool_name_allowed("file_read", Some("plan"), &[]));
        assert!(child_tool_name_allowed(
            "file_write",
            Some("implement"),
            &[]
        ));
        assert!(!child_tool_name_allowed("file_write", Some("plan"), &[]));
        assert!(!child_tool_name_allowed(
            "terminal_exec",
            Some("implement"),
            &[]
        ));
        assert!(!child_tool_name_allowed(
            "agent_spawn",
            Some("implement"),
            &[]
        ));
        assert!(!child_tool_name_allowed(
            "file_delete",
            Some("implement"),
            &[]
        ));
        assert!(child_tool_name_allowed(
            "file_read",
            None,
            &["file_read".into()]
        ));
    }

    #[test]
    fn child_provider_is_inferred_from_model_prefix() {
        assert_eq!(
            infer_child_provider("deepseek-v4-flash", ProviderPreset::Qwen),
            ProviderPreset::DeepSeek
        );
        assert_eq!(
            infer_child_provider("qwen3.5-flash", ProviderPreset::DeepSeek),
            ProviderPreset::Qwen
        );
        assert_eq!(
            infer_child_provider("gpt-5-mini", ProviderPreset::DeepSeek),
            ProviderPreset::OpenAi
        );
        // A provider that already hosts the model keeps it.
        assert_eq!(
            infer_child_provider("deepseek-v4-flash", ProviderPreset::Volcano),
            ProviderPreset::Volcano
        );
        assert_eq!(
            infer_child_provider("deepseek-v4-flash", ProviderPreset::DeepSeek),
            ProviderPreset::DeepSeek
        );
    }

    #[test]
    fn child_model_validation_allows_unknown_full_ids_and_catches_mismatches() {
        let mut qwen = ProviderPreset::Qwen.defaults();
        qwen.model = "qwen3.5-flash".into();
        assert!(validate_child_model(&qwen).is_ok());

        let mut qwen_wrong = ProviderPreset::Qwen.defaults();
        qwen_wrong.model = "deepseek-v4-flash".into();
        let error = validate_child_model(&qwen_wrong).unwrap_err();
        assert!(error.contains("belongs to DeepSeek"));
        assert!(error.contains("provider=deepseek"));

        let mut openai_shorthand = ProviderPreset::OpenAi.defaults();
        openai_shorthand.model = "v4pro".into();
        assert!(validate_child_model(&openai_shorthand).is_err());
    }

    #[test]
    fn child_title_prefers_explicit_then_role_with_prompt_snippet() {
        let arguments = ChildArgs {
            prompt: "review the database schema carefully".into(),
            max_turns: None,
            role: Some("reviewer".into()),
            model: None,
            provider: None,
            agent: None,
            title: None,
        };
        assert_eq!(
            child_title(&arguments, None, Some("reviewer")),
            "reviewer·review the databas…"
        );

        let arguments = ChildArgs {
            title: Some("自定义标题".into()),
            ..arguments
        };
        assert_eq!(
            child_title(&arguments, None, Some("reviewer")),
            "自定义标题"
        );
    }

    #[tokio::test]
    async fn child_agent_uses_configured_agent_template() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        storage
            .append_message(&session_id, Role::User, "parent")
            .unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let provider = OpenAiClient::scripted(vec![vec![
            ModelEvent::TextDelta("review done".into()),
            ModelEvent::Done,
        ]])
        .unwrap();
        let runner = AgentRunner::new(
            provider,
            ProviderPreset::OpenAi.defaults(),
            tools,
            storage.clone(),
            session_id.clone(),
        )
        .with_configured_agents(vec![AgentConfig {
            name: "reviewer".into(),
            mode: crate::commands::AgentMode::Explore,
            max_turns: 2,
            allowed_tools: vec!["file_read".into()],
            system_prompt: "Review for correctness.".into(),
        }]);

        let call = ToolCall {
            id: "call-agent".into(),
            name: "agent_spawn".into(),
            arguments: serde_json::json!({"prompt":"review the code","agent":"reviewer"}),
        };
        let (ui_events, mut receiver) = mpsc::channel(16);
        let result = runner.run_child(&call, &ui_events).await.unwrap();
        let payload: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["output"], "review done");
        assert_eq!(payload["title"], "reviewer");

        let sessions = storage.list_sessions(temp.path()).unwrap();
        let child = sessions.iter().find(|s| s.id != session_id).unwrap();
        assert_eq!(storage.session_mode(&child.id).unwrap(), "explore");
        assert_eq!(
            storage.session_child_role(&child.id).unwrap().as_deref(),
            Some("reviewer")
        );
        // The child session must be created before the result is returned.
        assert!(receiver.recv().await.is_some());
    }

    #[tokio::test]
    async fn child_agent_rejects_invalid_model_name() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let runner = AgentRunner::new(
            OpenAiClient::scripted(Vec::new()).unwrap(),
            ProviderPreset::OpenAi.defaults(),
            tools,
            storage,
            session_id,
        );
        let call = ToolCall {
            id: "bad-model".into(),
            name: "agent_spawn".into(),
            arguments: serde_json::json!({"prompt":"x","model":"v4pro"}),
        };
        let (ui_events, _receiver) = mpsc::channel(16);
        let error = runner.run_child(&call, &ui_events).await.unwrap_err();
        assert!(error.contains("unknown model"));
        assert!(error.contains("gpt-5-mini"));
    }

    #[tokio::test]
    async fn child_agent_executes_read_tools_across_turns() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("plan.txt"), "plan content").unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        storage
            .append_message(&session_id, Role::User, "parent")
            .unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let provider = OpenAiClient::scripted(vec![
            vec![
                ModelEvent::ToolCallComplete(ToolCall {
                    id: "c1".into(),
                    name: "file_read".into(),
                    arguments: serde_json::json!({"path":"plan.txt"}),
                }),
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::TextDelta("read and planned".into()),
                ModelEvent::Done,
            ],
        ])
        .unwrap();
        let runner = AgentRunner::new(
            provider,
            ProviderPreset::OpenAi.defaults(),
            tools,
            storage.clone(),
            session_id.clone(),
        );
        let call = ToolCall {
            id: "spawn".into(),
            name: "agent_spawn".into(),
            arguments: serde_json::json!({"prompt":"read plan.txt then plan"}),
        };
        let (ui_events, _receiver) = mpsc::channel(16);
        let result = runner.run_child(&call, &ui_events).await.unwrap();
        assert!(result.contains("read and planned"));

        let sessions = storage.list_sessions(temp.path()).unwrap();
        let child = sessions.iter().find(|s| s.id != session_id).unwrap();
        let messages = storage.load_messages(&child.id).unwrap();
        assert!(
            messages
                .iter()
                .any(|item| matches!(item, ConversationItem::ToolOutput { .. }))
        );
    }

    #[tokio::test]
    async fn child_agent_implement_role_writes_files_with_approval() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("plan.txt"), "plan").unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        storage
            .append_message(&session_id, Role::User, "parent")
            .unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let provider = OpenAiClient::scripted(vec![
            vec![
                ModelEvent::ToolCallComplete(ToolCall {
                    id: "c1".into(),
                    name: "file_read".into(),
                    arguments: serde_json::json!({"path":"plan.txt"}),
                }),
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCallComplete(ToolCall {
                    id: "c2".into(),
                    name: "file_write".into(),
                    arguments: serde_json::json!({"path":"out.txt","content":"written"}),
                }),
                ModelEvent::Done,
            ],
            vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done],
        ])
        .unwrap();
        let runner = AgentRunner::new(
            provider,
            ProviderPreset::OpenAi.defaults(),
            tools,
            storage.clone(),
            session_id.clone(),
        );

        let (ui_events, mut receiver) = mpsc::channel(16);
        let approver = tokio::spawn(async move {
            let mut statuses = Vec::new();
            while let Some(event) = receiver.recv().await {
                match event {
                    AgentEvent::Approval { reply, .. } => {
                        let _ = reply.send(true);
                    }
                    AgentEvent::ChildSessionProgress { progress, .. } => {
                        statuses.push(progress.status);
                    }
                    _ => {}
                }
            }
            statuses
        });

        let call = ToolCall {
            id: "impl".into(),
            name: "agent_spawn".into(),
            arguments: serde_json::json!({"prompt":"read plan.txt then write out.txt","role":"implement"}),
        };
        let result = runner.run_child(&call, &ui_events).await.unwrap();
        assert!(result.contains("done"));
        assert!(temp.path().join("out.txt").exists());
        drop(ui_events);
        drop(runner);
        let statuses = approver.await.unwrap();
        let approval_slot = statuses
            .iter()
            .position(|status| *status == ChildSessionStatus::WaitingApprovalSlot)
            .unwrap();
        let user_approval = statuses
            .iter()
            .position(|status| *status == ChildSessionStatus::WaitingApproval)
            .unwrap();
        assert!(approval_slot < user_approval);
    }

    #[tokio::test]
    async fn duplicate_tool_call_is_skipped_and_conversation_continues() {
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"fixture.txt"}),
        };
        let provider = OpenAiClient::scripted(vec![
            vec![
                ModelEvent::ToolCallComplete(call("call-1")),
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCallComplete(call("call-2")),
                ModelEvent::Done,
            ],
            vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done],
        ])
        .unwrap();
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("fixture.txt"), "fixture result").unwrap();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(temp.path()).unwrap();
        storage
            .append_message(&session_id, Role::User, "read fixture")
            .unwrap();
        let tools = Arc::new(ToolRegistry::new(
            Workspace::new(temp.path()).unwrap(),
            RuntimeConfig::default(),
            false,
        ));
        let runner = AgentRunner::new(
            provider,
            ProviderPreset::Custom.defaults(),
            tools,
            storage,
            session_id,
        );
        let (events, mut receiver) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            runner
                .run(
                    vec![ConversationItem::Message {
                        role: Role::User,
                        content: "read fixture".into(),
                    }],
                    events,
                )
                .await;
        });

        let mut starts = 0;
        let mut duplicate_notice = false;
        let mut completed = false;
        while let Some(event) = receiver.recv().await {
            match event {
                AgentEvent::ToolStarted(_) => starts += 1,
                AgentEvent::ToolFinished { result, .. } => {
                    duplicate_notice |= result.starts_with("Duplicate tool call was not executed");
                }
                AgentEvent::Completed { .. } => completed = true,
                AgentEvent::Failed(error) => panic!("unexpected failure: {error}"),
                _ => {}
            }
        }
        task.await.unwrap();
        assert_eq!(starts, 1);
        assert!(duplicate_notice);
        assert!(completed);
    }
}
