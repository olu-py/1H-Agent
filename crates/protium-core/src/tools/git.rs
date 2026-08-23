use serde::Deserialize;
use serde_json::Value;

use super::{
    ToolError,
    process::{self, ExecArgs},
};
use crate::{config::RuntimeConfig, security::Workspace};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitArgs {
    args: Vec<String>,
    #[serde(default = "default_cwd")]
    cwd: String,
    timeout_seconds: Option<u64>,
}

fn default_cwd() -> String {
    ".".into()
}

pub async fn execute(
    workspace: &Workspace,
    value: &Value,
    runtime: &RuntimeConfig,
) -> Result<String, ToolError> {
    let args: GitArgs = serde_json::from_value(value.clone())?;
    if args.args.is_empty() {
        return Err(ToolError::Execution("git requires a subcommand".into()));
    }
    if args
        .args
        .iter()
        .any(|arg| arg == "-C" || arg.starts_with("--git-dir") || arg.starts_with("--work-tree"))
    {
        return Err(ToolError::Security(
            "git directory override arguments are not allowed".into(),
        ));
    }
    process::execute_args(
        workspace,
        ExecArgs {
            program: "git".into(),
            args: args.args,
            cwd: args.cwd,
            timeout_seconds: args.timeout_seconds,
        },
        runtime,
    )
    .await
}
