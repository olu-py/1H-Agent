use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

use super::ToolError;
use crate::config::{BrowserConfig, McpServerConfig};
use crate::tools::web;

#[derive(Clone, Debug)]
pub struct ExternalTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub server: String,
}

pub async fn list_tools(config: &McpServerConfig) -> Result<Vec<ExternalTool>, ToolError> {
    let response = call_stdio(
        &config.command,
        &config.args,
        "tools/list",
        Value::Null,
        config.timeout_seconds,
        config.max_output_bytes,
    )
    .await?;
    let tools = response
        .get("result")
        .and_then(|value| value.get("tools"))
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::Execution("MCP tools/list returned no tools".into()))?;
    Ok(tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            Some(ExternalTool {
                name: format!("mcp:{}:{name}", config.name),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("external local tool")
                    .chars()
                    .take(512)
                    .collect(),
                parameters: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object"})),
                server: config.name.clone(),
            })
        })
        .take(64)
        .collect())
}

pub async fn call_mcp(
    config: &McpServerConfig,
    tool_name: &str,
    arguments: Value,
) -> Result<String, ToolError> {
    let response = call_stdio(
        &config.command,
        &config.args,
        "tools/call",
        json!({"name": tool_name, "arguments": arguments}),
        config.timeout_seconds,
        config.max_output_bytes,
    )
    .await?;
    format_result(response, config.max_output_bytes)
}

pub async fn call_browser(
    config: &BrowserConfig,
    operation: &str,
    arguments: Value,
    allow_private: bool,
) -> Result<String, ToolError> {
    if !config.enabled || config.command.trim().is_empty() {
        return Err(ToolError::Execution(
            "browser bridge is disabled or has no command".into(),
        ));
    }
    if operation == "browser_open" {
        let url = arguments
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        web::validate_public_url(url, allow_private).await?;
    }
    let response = call_stdio(
        &config.command,
        &config.args,
        operation,
        arguments,
        config.timeout_seconds,
        config.max_output_bytes,
    )
    .await?;
    format_result(response, config.max_output_bytes)
}

async fn call_stdio(
    command: &str,
    args: &[String],
    method: &str,
    params: Value,
    timeout_seconds: u64,
    max_output_bytes: usize,
) -> Result<Value, ToolError> {
    if command.trim().is_empty() {
        return Err(ToolError::Execution(
            "external command must not be empty".into(),
        ));
    }
    let timeout_seconds = timeout_seconds.clamp(1, 3600);
    let output_limit = max_output_bytes.clamp(1024, 8 * 1024 * 1024);
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(format!("{}\n", request).as_bytes())
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let _ = stdin.shutdown().await;
    }
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("external process has no stdout".into()))?;
    let mut bytes = Vec::with_capacity(output_limit.min(64 * 1024));
    let overflow = timeout(Duration::from_secs(timeout_seconds), async {
        let mut buffer = [0u8; 8192];
        loop {
            let count = stdout
                .read(&mut buffer)
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            if count == 0 {
                return Ok::<bool, ToolError>(false);
            }
            let remaining = output_limit.saturating_sub(bytes.len());
            if count > remaining {
                bytes.extend_from_slice(&buffer[..remaining]);
                return Ok(true);
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
    })
    .await
    .map_err(|_| ToolError::Execution("external tool timed out".into()))??;
    if overflow {
        let _ = child.kill().await;
        return Err(ToolError::Execution(
            "external tool output exceeded limit".into(),
        ));
    }
    let status = timeout(Duration::from_secs(1), child.wait())
        .await
        .map_err(|_| ToolError::Execution("external tool did not exit".into()))?
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if !status.success() {
        return Err(ToolError::Execution(format!(
            "external tool exited with {}",
            status
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string())
        )));
    }
    let text = String::from_utf8_lossy(&bytes);
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    serde_json::from_str(line)
        .map_err(|error| ToolError::Execution(format!("invalid external JSON response: {error}")))
}

fn format_result(response: Value, max_output_bytes: usize) -> Result<String, ToolError> {
    if let Some(error) = response.get("error") {
        return Err(ToolError::Execution(format!(
            "external tool error: {error}"
        )));
    }
    let value = response.get("result").cloned().unwrap_or(response);
    let mut output = serde_json::to_string_pretty(&value)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if output.len() > max_output_bytes {
        output.truncate(max_output_bytes);
        output.push_str("\n[output truncated]");
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exchanges_bounded_jsonl_with_a_local_process() {
        let response = call_stdio(
            "sh",
            &[
                "-c".into(),
                "read line; printf '{\"result\":{\"ok\":true}}\\n'".into(),
            ],
            "tools/list",
            Value::Null,
            5,
            4096,
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["ok"], true);
    }
}
