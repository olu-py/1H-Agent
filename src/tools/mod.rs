mod filesystem;
mod git;
mod process;
mod web;

use std::sync::Arc;

use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    config::RuntimeConfig,
    provider::{ToolCall, ToolDefinition},
    security::{PolicyDecision, Workspace, classify_tool},
};

#[derive(Clone)]
pub struct ToolRegistry {
    workspace: Workspace,
    runtime: RuntimeConfig,
    allow_private_networks: bool,
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
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn policy(&self, call: &ToolCall) -> PolicyDecision {
        classify_tool(&call.name, &call.arguments)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
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
                "web_fetch",
                "Fetch a public HTTP or HTTPS resource",
                json!({
                    "type":"object","properties":{"url":{"type":"string"},"method":{"type":"string","enum":["GET","HEAD"]}},"required":["url"],"additionalProperties":false
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
                "git",
                "Run Git with an argument vector in the workspace repository",
                json!({
                    "type":"object","properties":{"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":3600}},"required":["args"],"additionalProperties":false
                }),
            ),
        ]
    }

    pub async fn execute(&self, call: &ToolCall) -> Result<String, ToolError> {
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
            "terminal_exec" => {
                process::execute(&self.workspace, &call.arguments, &self.runtime).await
            }
            "git" => git::execute(&self.workspace, &call.arguments, &self.runtime).await,
            name => Err(ToolError::Unknown(name.to_owned())),
        }
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
