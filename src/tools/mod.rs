mod external;
mod filesystem;
mod git;
mod process;
mod web;

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    commands::AgentMode,
    config::{BrowserConfig, McpServerConfig, RuntimeConfig},
    provider::{ToolCall, ToolDefinition},
    security::{PolicyDecision, Workspace, classify_tool},
};

#[derive(Clone)]
pub struct ToolRegistry {
    workspace: Workspace,
    runtime: RuntimeConfig,
    allow_private_networks: bool,
    mode: Arc<RwLock<AgentMode>>,
    permissions: Arc<RwLock<BTreeMap<String, String>>>,
    browser: Arc<RwLock<Option<BrowserConfig>>>,
    mcp_servers: Arc<RwLock<Vec<McpServerConfig>>>,
    external_tools: Arc<RwLock<Vec<external::ExternalTool>>>,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("tool is not available: {0}")]
    Unknown(String),
    #[error("security policy denied the operation: {0}")]
    Security(String),
    #[error("tool failed: {0}")]
    Execution(String),
}

impl ToolRegistry {
    pub fn new(workspace: Workspace, runtime: RuntimeConfig, allow_private_networks: bool) -> Self {
        Self {
            workspace,
            runtime,
            allow_private_networks,
            mode: Arc::new(RwLock::new(AgentMode::Build)),
            permissions: Arc::new(RwLock::new(BTreeMap::new())),
            browser: Arc::new(RwLock::new(None)),
            mcp_servers: Arc::new(RwLock::new(Vec::new())),
            external_tools: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn policy(&self, call: &ToolCall) -> PolicyDecision {
        let mode = self.mode.read().map(|value| *value).unwrap_or_default();
        if mode != AgentMode::Build
            && matches!(
                call.name.as_str(),
                "file_write"
                    | "file_mkdir"
                    | "file_copy"
                    | "file_move"
                    | "file_delete"
                    | "terminal_exec"
                    | "terminal_shell"
            )
        {
            return PolicyDecision::Deny(format!("{} mode is read-only", mode.as_str()));
        }
        if call.name.starts_with("browser_") {
            return if matches!(call.name.as_str(), "browser_open" | "browser_snapshot") {
                PolicyDecision::Allow
            } else {
                PolicyDecision::RequireApproval("browser interaction can change page state".into())
            };
        }
        if call.name.starts_with("mcp:") {
            return PolicyDecision::RequireApproval("external local tool call".into());
        }
        let decision = classify_tool(&call.name, &call.arguments);
        let override_value = self.permissions.read().ok().and_then(|rules| {
            rules
                .get(&call.name)
                .cloned()
                .or_else(|| rules.get("*").cloned())
        });
        let decision = match (override_value.as_deref(), decision) {
            (Some("deny"), _) => PolicyDecision::Deny("denied by configured tool policy".into()),
            (Some("ask"), PolicyDecision::Allow) => {
                PolicyDecision::RequireApproval("requested by configured tool policy".into())
            }
            (_, decision) => decision,
        };
        if mode != AgentMode::Build
            && call.name == "git"
            && matches!(decision, PolicyDecision::RequireApproval(_))
        {
            return PolicyDecision::Deny(format!(
                "{} mode blocks mutating git operations",
                mode.as_str()
            ));
        }
        decision
    }

    pub fn set_mode(&self, mode: AgentMode) {
        if let Ok(mut current) = self.mode.write() {
            *current = mode;
        }
    }

    pub fn set_permission_rules(&self, rules: BTreeMap<String, String>) {
        if let Ok(mut current) = self.permissions.write() {
            *current = rules;
        }
    }

    pub fn mode(&self) -> AgentMode {
        self.mode.read().map(|value| *value).unwrap_or_default()
    }

    pub fn set_external_config(&self, browser: BrowserConfig, servers: Vec<McpServerConfig>) {
        if let Ok(mut current) = self.browser.write() {
            *current = Some(browser);
        }
        if let Ok(mut current) = self.mcp_servers.write() {
            *current = servers;
        }
    }

    pub async fn initialize_mcp(&self) -> Result<(), ToolError> {
        let servers = self
            .mcp_servers
            .read()
            .map_err(|_| ToolError::Execution("MCP configuration lock is poisoned".into()))?
            .iter()
            .filter(|server| server.enabled && !server.command.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>();
        let mut discovered = Vec::new();
        for server in servers {
            discovered.extend(external::list_tools(&server).await?);
        }
        if let Ok(mut tools) = self.external_tools.write() {
            *tools = discovered;
        }
        Ok(())
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = vec![
            definition(
                "file_list",
                "List entries in a workspace directory",
                json!({
                    "type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false
                }),
            ),
            definition(
                "file_stat",
                "Read metadata for a workspace path",
                path_schema(),
            ),
            definition(
                "file_read",
                "Read a UTF-8 text file from the workspace",
                json!({
                    "type":"object","properties":{"path":{"type":"string"},"max_bytes":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false
                }),
            ),
            definition(
                "file_search",
                "Search text files under a workspace directory",
                json!({
                    "type":"object","properties":{"path":{"type":"string"},"query":{"type":"string"},"max_results":{"type":"integer","minimum":1,"maximum":1000}},"required":["path","query"],"additionalProperties":false
                }),
            ),
            definition("file_mkdir", "Create a workspace directory", path_schema()),
            definition(
                "file_write",
                "Write a UTF-8 text file in the workspace",
                json!({
                    "type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false
                }),
            ),
            definition(
                "file_copy",
                "Copy a workspace file",
                source_destination_schema(),
            ),
            definition(
                "file_move",
                "Move a workspace path",
                source_destination_schema(),
            ),
            definition(
                "file_delete",
                "Delete a workspace file or empty directory",
                path_schema(),
            ),
            definition(
                "web_search",
                "Search the public web for current information using a bounded text-only endpoint",
                json!({
                    "type":"object","properties":{"query":{"type":"string"},"max_results":{"type":"integer","minimum":1,"maximum":10}},"required":["query"],"additionalProperties":false
                }),
            ),
            definition(
                "web_fetch",
                "Fetch a public HTTP or HTTPS resource",
                json!({
                    "type":"object","properties":{"url":{"type":"string"},"method":{"type":"string","enum":["GET","HEAD"]},"max_bytes":{"type":"integer","minimum":1024}},"required":["url"],"additionalProperties":false
                }),
            ),
            definition(
                "terminal_exec",
                "Run a program with an argument vector in the workspace",
                json!({
                    "type":"object","properties":{"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":3600}},"required":["program"],"additionalProperties":false
                }),
            ),
            definition(
                "agent_spawn",
                "Run one bounded child agent for a focused subtask",
                json!({"type":"object","properties":{"prompt":{"type":"string"},"max_turns":{"type":"integer","minimum":1,"maximum":3}},"required":["prompt"],"additionalProperties":false}),
            ),
            definition(
                "git",
                "Run Git with an argument vector in the workspace repository",
                json!({
                    "type":"object","properties":{"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":3600}},"required":["args"],"additionalProperties":false
                }),
            ),
            definition(
                "git_diff",
                "Read the current repository diff without changing files",
                json!({"type":"object","properties":{"cwd":{"type":"string"}},"additionalProperties":false}),
            ),
        ];
        if self
            .browser
            .read()
            .ok()
            .and_then(|browser| browser.as_ref().map(|value| value.enabled))
            .unwrap_or(false)
        {
            definitions.extend([
                definition("browser_open", "Open a public URL in an external browser bridge", json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"],"additionalProperties":false})),
                definition("browser_snapshot", "Read a text snapshot from the current browser page", json!({"type":"object","properties":{},"additionalProperties":false})),
                definition("browser_click", "Click a selector in the external browser", json!({"type":"object","properties":{"selector":{"type":"string"}},"required":["selector"],"additionalProperties":false})),
                definition("browser_type", "Type text into a selector in the external browser", json!({"type":"object","properties":{"selector":{"type":"string"},"text":{"type":"string"}},"required":["selector","text"],"additionalProperties":false})),
                definition("browser_press", "Press a key in the external browser", json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false})),
            ]);
        }
        if let Ok(tools) = self.external_tools.read() {
            definitions.extend(
                tools
                    .iter()
                    .map(|tool| definition(&tool.name, &tool.description, tool.parameters.clone())),
            );
        }
        definitions
    }

    pub async fn execute(&self, call: &ToolCall) -> Result<String, ToolError> {
        if let Some(tool) = self
            .external_tools
            .read()
            .ok()
            .and_then(|tools| tools.iter().find(|tool| tool.name == call.name).cloned())
        {
            let config = self
                .mcp_servers
                .read()
                .ok()
                .and_then(|servers| {
                    servers
                        .iter()
                        .find(|server| server.name == tool.server)
                        .cloned()
                })
                .ok_or_else(|| ToolError::Unknown(tool.name.clone()))?;
            let original_name = call.name.rsplit(':').next().unwrap_or(&call.name);
            return external::call_mcp(&config, original_name, call.arguments.clone()).await;
        }
        match call.name.as_str() {
            "file_list" => filesystem::list(&self.workspace, &call.arguments),
            "file_stat" => filesystem::stat(&self.workspace, &call.arguments),
            "file_read" => filesystem::read(
                &self.workspace,
                &call.arguments,
                self.runtime.max_tool_output_bytes,
            ),
            "file_search" => filesystem::search(
                &self.workspace,
                &call.arguments,
                self.runtime.max_tool_output_bytes,
            ),
            "file_mkdir" => filesystem::mkdir(&self.workspace, &call.arguments),
            "file_write" => filesystem::write(&self.workspace, &call.arguments),
            "file_copy" => filesystem::copy(&self.workspace, &call.arguments),
            "file_move" => filesystem::move_path(&self.workspace, &call.arguments),
            "file_delete" => filesystem::delete(&self.workspace, &call.arguments),
            "web_fetch" => {
                web::fetch(
                    &call.arguments,
                    self.runtime.max_fetch_bytes,
                    self.allow_private_networks,
                )
                .await
            }
            "web_search" => {
                web::search(
                    &call.arguments,
                    self.runtime.max_tool_output_bytes,
                    self.allow_private_networks,
                )
                .await
            }
            "terminal_exec" => {
                process::execute(&self.workspace, &call.arguments, &self.runtime).await
            }
            "git" => git::execute(&self.workspace, &call.arguments, &self.runtime).await,
            "git_diff" => {
                git::execute(
                    &self.workspace,
                    &json!({"args":["diff","--no-ext-diff","--unified=3"]}),
                    &self.runtime,
                )
                .await
            }
            name if name.starts_with("browser_") => {
                let browser = self
                    .browser
                    .read()
                    .ok()
                    .and_then(|value| value.clone())
                    .ok_or_else(|| ToolError::Unknown(name.to_owned()))?;
                external::call_browser(
                    &browser,
                    name,
                    call.arguments.clone(),
                    self.allow_private_networks,
                )
                .await
            }
            name => Err(ToolError::Unknown(name.to_owned())),
        }
    }

    pub async fn execute_shell(&self, command: &str) -> Result<String, ToolError> {
        process::execute_shell(&self.workspace, command, &self.runtime).await
    }
}

pub type SharedToolRegistry = Arc<ToolRegistry>;

fn definition(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
    }
}

fn path_schema() -> Value {
    json!({
        "type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false
    })
}

fn source_destination_schema() -> Value {
    json!({
        "type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"}},"required":["source","destination"],"additionalProperties":false
    })
}
