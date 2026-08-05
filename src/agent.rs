use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{
    config::{ProviderConfig, RuntimeConfig},
    provider::{ConversationItem, ModelEvent, ModelRequest, OpenAiClient, Role, ToolCall, Usage},
    security::PolicyDecision,
    storage::Storage,
    tools::SharedToolRegistry,
};

#[derive(Debug)]
pub enum AgentEvent {
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
        if let Err(error) = self.run_inner(&mut items, &ui_events).await {
            let _ = ui_events.send(AgentEvent::Failed(error)).await;
        }
    }

    async fn run_inner(
        &self,
        items: &mut Vec<ConversationItem>,
        ui_events: &mpsc::Sender<AgentEvent>,
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

        for _turn in 0..self.runtime.max_agent_turns {
            let request_items = if previous_response_id.is_some() {
                items[request_cursor..].to_vec()
            } else {
                items.clone()
            };
            let request = ModelRequest {
                kind: self.provider_config.kind,
                model: self.provider_config.model.clone(),
                items: request_items,
                tools: self.tools.definitions(),
                previous_response_id: previous_response_id.clone(),
            };
            let (model_tx, mut model_rx) = mpsc::channel(128);
            let provider = self.provider.clone();
            let provider_task =
                tokio::spawn(async move { provider.stream(request, model_tx).await });

            let mut assistant_text = String::new();
            let mut partials: HashMap<String, PartialToolCall> = HashMap::new();
            let mut completed_calls = Vec::new();
            let mut completed_ids = HashSet::new();
            let mut saw_done = false;
            while let Some(event) = model_rx.recv().await {
                match event {
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
                        }
                        self.storage
                            .save_response_id(&self.session_id, &id)
                            .map_err(|error| error.to_string())?;
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
                            call_id: call.id,
                            output: result,
                        });
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
                        answer.await.unwrap_or(false)
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
                    self.tools
                        .execute(&call)
                        .await
                        .unwrap_or_else(|error| error.to_string())
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
                    call_id: call.id,
                    output: result,
                });
            }
        }
        Err(format!(
            "agent stopped after {} tool turns",
            self.runtime.max_agent_turns
        ))
    }
}
