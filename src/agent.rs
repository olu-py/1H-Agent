use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    config::{
        NativeWebSearch, ProviderConfig, ProviderKind, ProviderPreset, ThinkingCapability,
        thinking_profile,
    },
    prompt,
    provider::{
        ConversationItem, ModelEvent, ModelRequest, OpenAiClient, Role, ThinkingMode, ToolCall,
        ToolDefinition, Usage,
    },
    security::PolicyDecision,
    storage::Storage,
    tools::SharedToolRegistry,
};

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
    LocalCommandFinished {
        command: String,
        result: String,
    },
}

#[derive(Clone)]
pub struct AgentRunner {
    provider: OpenAiClient,
    provider_config: ProviderConfig,
    tools: SharedToolRegistry,
    storage: Storage,
    session_id: String,
    approval_lock: Arc<Mutex<()>>,
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
        }
    }

    pub async fn run(&self, mut items: Vec<ConversationItem>, ui_events: mpsc::Sender<AgentEvent>) {
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
            items.len().saturating_sub(1)
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
                items.clone()
            };
            let mut request_items = request_items;
            if previous_response_id.is_none() {
                request_items.insert(
                    0,
                    ConversationItem::Message {
                        role: Role::System,
                        content: prompt::system_prompt(
                            self.provider_config.preset,
                            self.tools.mode(),
                        ),
                    },
                );
            }
            let request = ModelRequest {
                kind: self.provider_config.kind,
                model: self.provider_config.model.clone(),
                items: request_items,
                tools: self.tools.definitions(),
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
                for call in &spawn_tasks {
                    let runner = self.clone();
                    let call = call.clone();
                    let ui_events = ui_events.clone();
                    futures.push(async move {
                        let result = runner
                            .run_child(&call, &ui_events)
                            .await
                            .unwrap_or_else(|error| error);
                        (call.id, result)
                    });
                }
                let mut results: HashMap<String, String> = HashMap::new();
                while let Some((call_id, result)) = futures.next().await {
                    results.insert(call_id, result);
                }
                for call in spawn_tasks {
                    let result = results
                        .remove(&call.id)
                        .unwrap_or_else(|| "child agent did not produce a result".into());
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
        call: &ToolCall,
        ui_events: &mpsc::Sender<AgentEvent>,
    ) -> String {
        match self.tools.policy(call) {
            PolicyDecision::Allow => self
                .tools
                .execute(call)
                .await
                .unwrap_or_else(|error| error.to_string()),
            PolicyDecision::Deny(reason) => format!("denied by policy: {reason}"),
            PolicyDecision::RequireApproval(reason) => {
                let _guard = self.approval_lock.lock().await;
                let (reply, answer) = oneshot::channel();
                if ui_events
                    .send(AgentEvent::Approval {
                        call: call.clone(),
                        reason,
                        reply,
                    })
                    .await
                    .is_err()
                {
                    return "approval channel closed".into();
                }
                match answer.await {
                    Ok(true) => self
                        .tools
                        .execute(call)
                        .await
                        .unwrap_or_else(|error| error.to_string()),
                    Ok(false) => "rejected by user".into(),
                    Err(_) => "approval cancelled".into(),
                }
            }
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
        let mut provider_config = self.provider_config.clone();
        let max_turns = arguments.max_turns.unwrap_or(3).clamp(1, 8);
        if let Some(model) = arguments
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            provider_config.model = model.to_owned();
        }
        // Reject shorthand/invalid model names up front so the orchestrator can
        // correct itself instead of surfacing a raw provider HTTP 400.
        let selectable = provider_config.preset.selectable_models();
        if !selectable.is_empty() && !selectable.contains(&provider_config.model.as_str()) {
            return Err(format!(
                "unknown model \"{}\" for {}; use a full model name such as {}",
                provider_config.model,
                provider_config.preset.label(),
                selectable.join(", ")
            ));
        }
        provider_config.use_previous_response_id = false;

        // Create a nested session so the child's work is inspectable from the
        // session panel, using its own model when one was requested.
        let workspace = self
            .storage
            .session_workspace(&self.session_id)
            .map_err(|error| error.to_string())?;
        let title = arguments
            .title
            .as_deref()
            .or(arguments.role.as_deref())
            .unwrap_or("子 Agent")
            .to_owned();
        let child_id = self
            .storage
            .create_child_session(
                Path::new(&workspace),
                &self.session_id,
                self.provider_config.preset.key_id(),
                &provider_config.model,
                &title,
            )
            .map_err(|error| error.to_string())?;
        self.storage
            .append_message(&child_id, Role::User, &arguments.prompt)
            .map_err(|error| error.to_string())?;
        // Let the UI refresh the session tree as soon as the child exists,
        // rather than waiting for the whole turn to complete.
        let _ = ui_events.send(AgentEvent::SessionsChanged).await;

        let tools: Vec<ToolDefinition> = self
            .tools
            .definitions()
            .into_iter()
            .filter(|tool| {
                matches!(
                    tool.name.as_str(),
                    "file_list"
                        | "file_stat"
                        | "file_read"
                        | "file_search"
                        | "web_search"
                        | "web_fetch"
                        | "git_diff"
                        | "file_write"
                        | "file_mkdir"
                        | "file_copy"
                        | "file_move"
                ) && (is_implement_role(arguments.role.as_deref())
                    || !matches!(
                        tool.name.as_str(),
                        "file_write" | "file_mkdir" | "file_copy" | "file_move"
                    ))
            })
            .collect();
        let thinking_profile_kind =
            thinking_profile(provider_config.preset, &provider_config.model).kind;
        let native_web_search = provider_config.preset == ProviderPreset::DeepSeek
            && provider_config.kind == ProviderKind::Responses
            && provider_config.native_web_search != NativeWebSearch::Disabled;

        // Multi-turn loop: unlike before, actually execute the read-only tools
        // the child requests, so file-reading subtasks no longer return empty.
        let mut items = vec![ConversationItem::Message {
            role: Role::User,
            content: arguments.prompt.clone(),
        }];
        let mut full_output = String::new();
        let mut tool_call_count = 0usize;
        let mut remaining_turns = max_turns;
        loop {
            if remaining_turns == 0 {
                let trail = summarize_child_trail(&items, 3, 512);
                full_output.push_str(&format!("\n[child agent reached its turn limit]{trail}"));
                break;
            }
            remaining_turns -= 1;

            let request = ModelRequest {
                kind: provider_config.kind,
                model: provider_config.model.clone(),
                items: items.clone(),
                tools: tools.clone(),
                previous_response_id: None,
                native_web_search,
                thinking_mode: ThinkingMode::Disabled,
                thinking_level: provider_config.thinking_level,
                thinking_budget_tokens: provider_config.thinking_budget_tokens,
                thinking_profile_kind,
            };
            let mut collector = StreamCollector::new(Some(256 * 1024));
            match stream_once(
                &self.provider,
                request,
                &mut collector,
                512,
                ui_events,
                |_| Ok(Forwarded::Ignore),
            )
            .await
            {
                Ok(()) => {}
                Err(StreamFailure::Provider(error)) => {
                    append_text_bounded(
                        &mut full_output,
                        &format!("\n[child failed: {error}]"),
                        256 * 1024,
                    );
                    break;
                }
                Err(StreamFailure::Handler(error)) | Err(StreamFailure::Join(error)) => {
                    return Err(error);
                }
                Err(StreamFailure::EndedWithoutCompletion) => {
                    append_text_bounded(
                        &mut full_output,
                        "\n[child stream ended without completion]",
                        256 * 1024,
                    );
                    break;
                }
            }
            append_text_bounded(&mut full_output, &collector.assistant_text, 256 * 1024);
            collector.finish_partials("child tool")?;
            if collector.completed_calls.is_empty() {
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
                let result = self.execute_child_tool(&tool_call, ui_events).await;
                self.storage
                    .append_tool_output(&child_id, &tool_call.id, &result)
                    .map_err(|error| error.to_string())?;
                items.push(ConversationItem::ToolOutput {
                    call_id: tool_call.id.clone(),
                    output: result,
                });
            }
        }

        if full_output.trim().is_empty() {
            if tool_call_count > 0 {
                full_output.push_str(&format!(
                    "[child agent issued {tool_call_count} tool call(s) but returned no text]"
                ));
            } else {
                full_output.push_str("[child agent returned no text]");
            }
        }
        self.storage
            .append_message(&child_id, Role::Assistant, &full_output)
            .map_err(|error| error.to_string())?;
        Ok(full_output)
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
            ThinkingMode::CompatibleAuto
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
    title: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::{config::RuntimeConfig, security::Workspace, tools::ToolRegistry};

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

        config = ProviderPreset::Volcano.defaults();
        assert_eq!(thinking_mode_for(&config), ThinkingMode::VolcanoChat);

        config = ProviderPreset::Custom.defaults();
        assert_eq!(thinking_mode_for(&config), ThinkingMode::CompatibleAuto);
        config.thinking = ThinkingCapability::Qwen;
        assert_eq!(thinking_mode_for(&config), ThinkingMode::QwenChat);
        config.thinking = ThinkingCapability::OpenAi;
        config.kind = ProviderKind::Responses;
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
        let (ui_events, _receiver) = mpsc::channel(16);
        let result = runner.run_child(&call, &ui_events).await.unwrap();
        assert_eq!(result, "child result");

        let sessions = storage.list_sessions(temp.path()).unwrap();
        assert_eq!(sessions.len(), 2);
        let child = sessions.iter().find(|s| s.id != session_id).unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(child.title, "plan");
        assert_eq!(
            storage.session_provider_model(&child.id).unwrap().1,
            "gpt-5"
        );
        assert_eq!(storage.load_messages(&child.id).unwrap().len(), 2);
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
            while let Some(event) = receiver.recv().await {
                if let AgentEvent::Approval { reply, .. } = event {
                    let _ = reply.send(true);
                }
            }
        });

        let call = ToolCall {
            id: "impl".into(),
            name: "agent_spawn".into(),
            arguments: serde_json::json!({"prompt":"read plan.txt then write out.txt","role":"implement"}),
        };
        let result = runner.run_child(&call, &ui_events).await.unwrap();
        assert!(result.contains("done"));
        assert!(temp.path().join("out.txt").exists());
        approver.abort();
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
