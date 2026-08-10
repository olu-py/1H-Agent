use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{
    config::{NativeWebSearch, ProviderConfig, ProviderKind, ProviderPreset, RuntimeConfig},
    prompt,
    provider::{ConversationItem, ModelEvent, ModelRequest, OpenAiClient, Role, ToolCall, Usage},
    security::PolicyDecision,
    storage::Storage,
    tools::SharedToolRegistry,
};

#[derive(Debug)]
pub enum AgentEvent {
    ThinkingSummary(String),
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
    LocalCommandFinished {
        command: String,
        result: String,
    },
}

#[derive(Clone)]
pub struct AgentRunner {
    provider: OpenAiClient,
    provider_config: ProviderConfig,
    runtime: RuntimeConfig,
    tools: SharedToolRegistry,
    storage: Storage,
    session_id: String,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl AgentRunner {
    pub fn new(
        provider: OpenAiClient,
        provider_config: ProviderConfig,
        runtime: RuntimeConfig,
        tools: SharedToolRegistry,
        storage: Storage,
        session_id: String,
    ) -> Self {
        Self {
            provider,
            provider_config,
            runtime,
            tools,
            storage,
            session_id,
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
        let mut next_summary = format!(
            "正在以 {} 模式分析请求，并选择下一项安全操作。",
            self.tools.mode().as_str().to_ascii_uppercase()
        );
        let native_web_search = self.provider_config.preset == ProviderPreset::DeepSeek
            && self.provider_config.kind == ProviderKind::Responses
            && self.provider_config.native_web_search != NativeWebSearch::Disabled;
        for _turn in 0..self.runtime.max_agent_turns {
            let summary = next_summary.clone();
            ui_events
                .send(AgentEvent::ThinkingSummary(summary.clone()))
                .await
                .map_err(|_| "UI event receiver closed".to_owned())?;
            items.push(ConversationItem::ThinkingSummary {
                content: summary.clone(),
            });
            self.storage
                .append_thinking_summary(&self.session_id, &summary)
                .map_err(|error| error.to_string())?;
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
            };
            let (model_tx, mut model_rx) = mpsc::channel(128);
            ui_events
                .send(AgentEvent::ModelStreaming)
                .await
                .map_err(|_| "UI event receiver closed".to_owned())?;
            let provider = self.provider.clone();
            let provider_task =
                tokio::spawn(async move { provider.stream(request, model_tx).await });

            let mut assistant_text = String::new();
            let mut partials: HashMap<String, PartialToolCall> = HashMap::new();
            let mut completed_calls = Vec::new();
            let mut completed_ids = HashSet::new();
            let mut saw_done = false;
            let mut search_results = 0usize;
            let mut search_bytes = 0usize;
            while let Some(event) = model_rx.recv().await {
                match event {
                    ModelEvent::WebSearchStarted { query } => {
                        ui_events
                            .send(AgentEvent::WebSearchStarted { query })
                            .await
                            .map_err(|_| "UI event receiver closed".to_owned())?;
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
                            ui_events
                                .send(AgentEvent::WebSearchResult {
                                    title,
                                    url,
                                    snippet,
                                })
                                .await
                                .map_err(|_| "UI event receiver closed".to_owned())?;
                        }
                    }
                    ModelEvent::WebSearchCompleted { count } => {
                        ui_events
                            .send(AgentEvent::WebSearchCompleted {
                                count: if count == 0 {
                                    search_results
                                } else {
                                    count.min(10)
                                },
                            })
                            .await
                            .map_err(|_| "UI event receiver closed".to_owned())?;
                    }
                    ModelEvent::ProviderItem(item) => {
                        let encoded = serde_json::to_vec(&item)
                            .map_err(|error| format!("invalid provider item: {error}"))?;
                        if encoded.len() <= 64 * 1024 {
                            items.push(ConversationItem::ProviderItem { item: item.clone() });
                            self.storage
                                .append_provider_item(&self.session_id, &item)
                                .map_err(|error| error.to_string())?;
                        }
                    }
                    ModelEvent::TextDelta(delta) => {
                        assistant_text.push_str(&delta);
                        ui_events
                            .send(AgentEvent::TextDelta(delta))
                            .await
                            .map_err(|_| "UI event receiver closed".to_owned())?;
                    }
                    ModelEvent::ToolCallDelta {
                        slot,
                        id,
                        name,
                        arguments_delta,
                    } => {
                        let partial = partials.entry(slot).or_default();
                        if let Some(id) = id {
                            partial.id = id;
                        }
                        if let Some(name) = name {
                            partial.name = name;
                        }
                        partial.arguments.push_str(&arguments_delta);
                    }
                    ModelEvent::ToolCallComplete(call) => {
                        completed_ids.insert(call.id.clone());
                        completed_calls.push(call);
                    }
                    ModelEvent::ResponseId(id) => {
                        if self.provider_config.use_previous_response_id {
                            previous_response_id = Some(id.clone());
                            self.storage
                                .save_response_id(&self.session_id, &id)
                                .map_err(|error| error.to_string())?;
                        }
                    }
                    ModelEvent::Usage(usage) => {
                        let _ = ui_events.send(AgentEvent::Usage(usage)).await;
                    }
                    ModelEvent::Done => {
                        saw_done = true;
                        break;
                    }
                }
            }
            let provider_result = provider_task.await.map_err(|error| error.to_string())?;
            if let Err(error) = provider_result {
                if previous_response_id.is_some()
                    && assistant_text.is_empty()
                    && partials.is_empty()
                    && completed_calls.is_empty()
                {
                    // Compatible endpoints can expire or reject server-side state.
                    // Replay the canonical local history once instead.
                    previous_response_id = None;
                    request_cursor = 0;
                    continue;
                }
                return Err(error.to_string());
            }
            if !saw_done {
                return Err("model stream ended without completion".into());
            }

            for partial in partials.into_values() {
                if completed_ids.contains(&partial.id) || partial.name.is_empty() {
                    continue;
                }
                let arguments: Value = serde_json::from_str(if partial.arguments.is_empty() {
                    "{}"
                } else {
                    &partial.arguments
                })
                .map_err(|error| format!("invalid tool arguments: {error}"))?;
                completed_calls.push(ToolCall {
                    id: if partial.id.is_empty() {
                        format!("call_{}", uuid::Uuid::new_v4())
                    } else {
                        partial.id
                    },
                    name: partial.name,
                    arguments,
                });
            }
            if !assistant_text.is_empty() {
                items.push(ConversationItem::Message {
                    role: Role::Assistant,
                    content: assistant_text.clone(),
                });
                self.storage
                    .append_message(&self.session_id, Role::Assistant, &assistant_text)
                    .map_err(|error| error.to_string())?;
            }
            if completed_calls.is_empty() {
                ui_events
                    .send(AgentEvent::Completed {
                        items: items.clone(),
                    })
                    .await
                    .map_err(|_| "UI event receiver closed".to_owned())?;
                return Ok(());
            }

            items.push(ConversationItem::AssistantToolCalls {
                calls: completed_calls.clone(),
            });
            self.storage
                .append_tool_calls(&self.session_id, &completed_calls)
                .map_err(|error| error.to_string())?;
            let tool_names = completed_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            // When Responses server state is enabled, the response already owns the
            // assistant text and tool calls. Only subsequent tool outputs are new.
            request_cursor = items.len();
            for call in completed_calls {
                let decision = self.tools.policy(&call);
                let approved = match decision {
                    PolicyDecision::Allow => true,
                    PolicyDecision::Deny(reason) => {
                        self.storage
                            .begin_tool(&self.session_id, &call, "denied")
                            .map_err(|error| error.to_string())?;
                        let result = format!("denied by policy: {reason}");
                        self.storage
                            .finish_tool(&call.id, &result)
                            .map_err(|error| error.to_string())?;
                        items.push(ConversationItem::ToolOutput {
                            call_id: call.id.clone(),
                            output: result.clone(),
                        });
                        self.storage
                            .append_tool_output(&self.session_id, &call.id, &result)
                            .map_err(|error| error.to_string())?;
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
                let result = if approved {
                    ui_events
                        .send(AgentEvent::ToolStarted(call.clone()))
                        .await
                        .map_err(|_| "UI event receiver closed".to_owned())?;
                    if call.name == "agent_spawn" {
                        if depth >= 1 {
                            "child agents cannot recursively spawn another child".into()
                        } else {
                            self.run_child(&call).await.unwrap_or_else(|error| error)
                        }
                    } else {
                        self.tools
                            .execute(&call)
                            .await
                            .unwrap_or_else(|error| error.to_string())
                    }
                } else {
                    "rejected by user".into()
                };
                self.storage
                    .finish_tool(&call.id, &result)
                    .map_err(|error| error.to_string())?;
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
            }
            next_summary = format!("正在检查 {tool_names} 的结果，并决定下一项安全操作。");
        }
        Err(format!(
            "maximum tool turns reached ({})",
            self.runtime.max_agent_turns
        ))
    }

    async fn run_child(&self, call: &ToolCall) -> Result<String, String> {
        let arguments: ChildArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| format!("invalid child agent arguments: {error}"))?;
        if arguments.prompt.trim().is_empty() {
            return Err("child agent prompt must not be empty".into());
        }
        let mut provider_config = self.provider_config.clone();
        let _max_turns = arguments.max_turns.unwrap_or(3).clamp(1, 3);
        provider_config.use_previous_response_id = false;
        let request = ModelRequest {
            kind: provider_config.kind,
            model: provider_config.model,
            items: vec![ConversationItem::Message {
                role: Role::User,
                content: arguments.prompt,
            }],
            tools: self
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
                    )
                })
                .collect(),
            previous_response_id: None,
            native_web_search: provider_config.preset == ProviderPreset::DeepSeek
                && provider_config.kind == ProviderKind::Responses
                && provider_config.native_web_search != NativeWebSearch::Disabled,
        };
        let (events, mut receiver) = mpsc::channel(512);
        let provider = self.provider.clone();
        let task = tokio::spawn(async move { provider.stream(request, events).await });
        let mut output = String::new();
        while let Some(event) = receiver.recv().await {
            match event {
                ModelEvent::TextDelta(delta) => {
                    let remaining = 256 * 1024usize - output.len();
                    if remaining > 0 {
                        let mut end = delta.len().min(remaining);
                        while end > 0 && !delta.is_char_boundary(end) {
                            end -= 1;
                        }
                        output.push_str(&delta[..end]);
                    }
                }
                ModelEvent::Done => break,
                ModelEvent::Usage(_)
                | ModelEvent::ResponseId(_)
                | ModelEvent::WebSearchStarted { .. }
                | ModelEvent::WebSearchResult { .. }
                | ModelEvent::WebSearchCompleted { .. }
                | ModelEvent::ProviderItem(_)
                | ModelEvent::ToolCallDelta { .. }
                | ModelEvent::ToolCallComplete(_) => {}
            }
        }
        let result = task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string());
        if let Err(error) = result {
            output.push_str(&format!("\n[child failed: {error}]"));
        }
        if output.is_empty() {
            output.push_str("[child agent returned no text]");
        }
        Ok(output)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildArgs {
    prompt: String,
    max_turns: Option<usize>,
}
