use std::{process::Command, sync::Arc};

use protium_agent::{
    config::RuntimeConfig,
    provider::ToolCall,
    security::{PolicyDecision, Workspace},
    tools::ToolRegistry,
};
use serde_json::json;
use tempfile::tempdir;

#[tokio::test]
async fn workspace_file_and_git_tools_work_end_to_end() {
    let root = tempdir().unwrap();
    let status = Command::new("git")
        .arg("init")
        .arg(root.path())
        .status()
        .unwrap();
    assert!(status.success());

    let workspace = Workspace::new(root.path()).unwrap();
    let registry = Arc::new(ToolRegistry::new(
        workspace,
        RuntimeConfig::default(),
        false,
    ));
    let write = ToolCall {
        id: "write_1".into(),
        name: "file_write".into(),
        arguments: json!({"path":"hello.txt","content":"hello"}),
    };
    assert!(matches!(
        registry.policy(&write),
        PolicyDecision::RequireApproval(_)
    ));
    registry.execute(&write).await.unwrap();

    let status = ToolCall {
        id: "git_1".into(),
        name: "git".into(),
        arguments: json!({"args":["status","--short"]}),
    };
    assert_eq!(registry.policy(&status), PolicyDecision::Allow);
    let result = registry.execute(&status).await.unwrap();
    assert!(result.contains("hello.txt"));
}

#[tokio::test]
async fn registry_rechecks_path_security_at_execution() {
    let root = tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let registry = ToolRegistry::new(workspace, RuntimeConfig::default(), false);
    let call = ToolCall {
        id: "read_1".into(),
        name: "file_read".into(),
        arguments: json!({"path":"../outside.txt"}),
    };
    assert!(registry.execute(&call).await.is_err());
}

#[tokio::test]
async fn file_glob_and_repo_map_are_allowed_read_only_tools() {
    let root = tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let registry = ToolRegistry::new(workspace, RuntimeConfig::default(), false);
    let write = ToolCall {
        id: "write_1".into(),
        name: "file_write".into(),
        arguments: json!({"path":"lib.rs","content":"pub fn run() {}\nstruct Config {}"}),
    };
    registry.execute(&write).await.unwrap();

    let glob_call = ToolCall {
        id: "glob_1".into(),
        name: "file_glob".into(),
        arguments: json!({"path":".","pattern":"*.rs"}),
    };
    assert_eq!(registry.policy(&glob_call), PolicyDecision::Allow);
    let glob_result = registry.execute(&glob_call).await.unwrap();
    assert!(glob_result.contains("lib.rs"));

    let map_call = ToolCall {
        id: "map_1".into(),
        name: "repo_map".into(),
        arguments: json!({"path":"."}),
    };
    assert_eq!(registry.policy(&map_call), PolicyDecision::Allow);
    let map_result = registry.execute(&map_call).await.unwrap();
    assert!(map_result.contains("pub fn run()"));
    assert!(map_result.contains("\"kind\": \"fn\""));
}
