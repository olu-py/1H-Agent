use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{
    config::{NativeWebSearch, ProviderConfig, ProviderKind, ProviderPreset, ThinkingCapability},
    prompt,
    provider::{
        ConversationItem, ModelEvent, ModelRequest, OpenAiClient, Role, ThinkingMode, ToolCall,
        Usage,
    },
    security::PolicyDecision,
    storage::Storage,
    tools::SharedToolRegistry,
};

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
                            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                                self.storage
                                    .append_provider_item(&self.session_id, &item)
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                    }
                    ModelEvent::ReasoningDelta(delta) => {
                        ui_events
                            .send(AgentEvent::ReasoningDelta(delta))
                            .await
                            .map_err(|_| "UI event receiver closed".to_owned())?;
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
            // When Responses server state is enabled, the response already owns the
            // assistant text and tool calls. Only subsequent tool outputs are new.
            request_cursor = items.len();
            for call in completed_calls {
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
                executed_tool_calls.insert(signature);
            }
        }
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
            thinking_mode: ThinkingMode::Disabled,
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
                ModelEvent::ReasoningDelta(_) => {}
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
