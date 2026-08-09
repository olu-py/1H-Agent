use std::{process::Stdio, time::Duration};

use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

use super::ToolError;
use crate::{config::RuntimeConfig, security::Workspace};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecArgs {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    pub timeout_seconds: Option<u64>,
}

fn default_cwd() -> String {
    ".".into()
}

pub async fn execute(
    workspace: &Workspace,
    value: &Value,
    runtime: &RuntimeConfig,
) -> Result<String, ToolError> {
    let args: ExecArgs = serde_json::from_value(value.clone())?;
    execute_args(workspace, args, runtime).await
}

pub async fn execute_args(
    workspace: &Workspace,
    args: ExecArgs,
    runtime: &RuntimeConfig,
) -> Result<String, ToolError> {
    if args.program.trim().is_empty() {
        return Err(ToolError::Execution("program must not be empty".into()));
    }
    let cwd = workspace
        .resolve_existing(&args.cwd)
        .map_err(|error| ToolError::Security(error.to_string()))?;
    let timeout_seconds = args
        .timeout_seconds
        .unwrap_or(runtime.command_timeout_seconds)
        .min(3600);
    let mut command = Command::new(&args.program);
    command
        .args(&args.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);

    let mut child = command
        .spawn()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let pid = child.id();
    let mut process_tree = ProcessTreeGuard::new(pid);
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let limit = runtime.max_tool_output_bytes;
    let stdout_task = tokio::spawn(read_capped(stdout, limit));
    let stderr_task = tokio::spawn(read_capped(stderr, limit));

    let status = match timeout(Duration::from_secs(timeout_seconds), child.wait()).await {
        Ok(status) => status.map_err(|error| ToolError::Execution(error.to_string()))?,
        Err(_) => {
            terminate_process_tree(pid).await;
            process_tree.disarm();
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ToolError::Execution(format!(
                "process timed out after {timeout_seconds} seconds"
            )));
        }
    };
    // A parent can exit while background descendants still hold the output
    // pipes. Terminate the now-unowned process group before joining readers.
    terminate_process_tree(pid).await;
    process_tree.disarm();
    let stdout = stdout_task
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))??;
    let stderr = stderr_task
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))??;
    Ok(format!(
        "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
        status
            .code()
            .map_or_else(|| "signal".into(), |code| code.to_string()),
        stdout,
        stderr
    ))
}

/// Execute an explicitly requested shell command. The caller must perform an
/// approval check before invoking this function.
pub async fn execute_shell(
    workspace: &Workspace,
    command: &str,
    runtime: &RuntimeConfig,
) -> Result<String, ToolError> {
    if command.trim().is_empty() {
        return Err(ToolError::Execution(
            "shell command must not be empty".into(),
        ));
    }
    let args = if cfg!(windows) {
        ExecArgs {
            program: "cmd".into(),
            args: vec!["/C".into(), command.into()],
            cwd: ".".into(),
            timeout_seconds: None,
        }
    } else {
        ExecArgs {
            program: "sh".into(),
            args: vec!["-c".into(), command.into()],
            cwd: ".".into(),
            timeout_seconds: None,
        }
    };
    execute_args(workspace, args, runtime).await
}

struct ProcessTreeGuard {
    pid: Option<u32>,
    armed: bool,
}

impl ProcessTreeGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        #[cfg(windows)]
        if let Some(pid) = self.pid {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<String, ToolError> {
    let mut result = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(result.len());
        if count > remaining {
            result.extend_from_slice(&buffer[..remaining]);
            truncated = true;
        } else if remaining > 0 {
            result.extend_from_slice(&buffer[..count]);
        } else {
            truncated = true;
        }
    }
    let mut text = String::from_utf8_lossy(&result).into_owned();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    Ok(text)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut Command) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    use std::os::windows::process::CommandExt;
    command
        .as_std_mut()
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
async fn terminate_process_tree(pid: Option<u32>) {
    if let Some(pid) = pid {
        // The child was placed in a process group whose id equals its pid.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(windows)]
async fn terminate_process_tree(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn captures_process_output() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let result = execute_args(
            &workspace,
            ExecArgs {
                program: if cfg!(windows) {
                    "cmd".into()
                } else {
                    "printf".into()
                },
                args: if cfg!(windows) {
                    vec!["/C".into(), "echo hello".into()]
                } else {
                    vec!["hello".into()]
                },
                cwd: ".".into(),
                timeout_seconds: Some(5),
            },
            &RuntimeConfig::default(),
        )
        .await
        .unwrap();
        assert!(result.contains("hello"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn abort_kills_descendant_process_group() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let runtime = RuntimeConfig::default();
        let task = tokio::spawn(async move {
            execute_args(
                &workspace,
                ExecArgs {
                    program: "sh".into(),
                    args: vec!["-c".into(), "sleep 30 & echo $! > child.pid; wait".into()],
                    cwd: ".".into(),
                    timeout_seconds: Some(30),
                },
                &runtime,
            )
            .await
        });

        let pid_path = root.path().join("child.pid");
        for _ in 0..50 {
            if pid_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid: i32 = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        task.abort();
        let _ = task.await;
        for _ in 0..50 {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("descendant process {pid} survived task cancellation");
    }
}
