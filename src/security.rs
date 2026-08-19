use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyDecision {
    Allow,
    RequireApproval(String),
    Deny(String),
}

#[derive(Debug, Error)]
pub enum SecurityError {
    #[error("path is outside the workspace: {0}")]
    OutsideWorkspace(String),
    #[error("path has no existing in-workspace parent: {0}")]
    MissingParent(String),
    #[error("cannot inspect path: {0}")]
    Io(#[from] std::io::Error),
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, SecurityError> {
        let root = fs::canonicalize(root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve_existing(&self, requested: impl AsRef<Path>) -> Result<PathBuf, SecurityError> {
        let joined = self.join_requested(requested.as_ref())?;
        let resolved = fs::canonicalize(&joined)?;
        self.ensure_inside(resolved)
    }

    pub fn resolve_new(&self, requested: impl AsRef<Path>) -> Result<PathBuf, SecurityError> {
        let joined = self.join_requested(requested.as_ref())?;
        if joined.exists() {
            return self.resolve_existing(joined);
        }

        let mut missing = Vec::new();
        let mut cursor = joined.as_path();
        while !cursor.exists() {
            let name = cursor
                .file_name()
                .ok_or_else(|| SecurityError::MissingParent(joined.display().to_string()))?;
            missing.push(name.to_os_string());
            cursor = cursor
                .parent()
                .ok_or_else(|| SecurityError::MissingParent(joined.display().to_string()))?;
        }

        let mut resolved = self.resolve_existing(cursor)?;
        for part in missing.iter().rev() {
            resolved.push(part);
        }
        self.ensure_inside(resolved)
    }

    fn join_requested(&self, requested: &Path) -> Result<PathBuf, SecurityError> {
        if requested
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(SecurityError::OutsideWorkspace(
                requested.display().to_string(),
            ));
        }
        Ok(if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        })
    }

    fn ensure_inside(&self, path: PathBuf) -> Result<PathBuf, SecurityError> {
        if path.starts_with(&self.root) {
            Ok(path)
        } else {
            Err(SecurityError::OutsideWorkspace(path.display().to_string()))
        }
    }
}

pub fn classify_tool(name: &str, arguments: &Value) -> PolicyDecision {
    match name {
        "file_list" | "file_stat" | "file_read" | "file_search" | "web_search" | "web_fetch"
        | "git_diff" | "todo_read" | "todo_write" => PolicyDecision::Allow,
        "file_write" | "file_mkdir" | "file_copy" | "file_move" | "file_delete" => {
            PolicyDecision::RequireApproval(format!("{name} changes workspace files"))
        }
        "terminal_exec" => PolicyDecision::RequireApproval("run a local process".into()),
        "agent_spawn" => PolicyDecision::RequireApproval("start a bounded child agent".into()),
        "git" => classify_git(arguments),
        _ => PolicyDecision::Deny(format!("unknown tool: {name}")),
    }
}

fn classify_git(arguments: &Value) -> PolicyDecision {
    let operation = arguments
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| args.first())
        .and_then(Value::as_str)
        .unwrap_or_default();
    match operation {
        "status" | "diff" | "log" | "show" => PolicyDecision::Allow,
        "add" | "commit" | "checkout" | "switch" | "branch" | "restore" | "reset" | "fetch"
        | "pull" | "push" => {
            PolicyDecision::RequireApproval(format!("git {operation} can change state"))
        }
        "" => PolicyDecision::Deny("git requires a subcommand".into()),
        other => PolicyDecision::RequireApproval(format!("unclassified git operation: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn blocks_parent_traversal() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        assert!(workspace.resolve_new("../outside").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn blocks_symlink_escape() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        assert!(workspace.resolve_existing("escape").is_err());
        assert!(workspace.resolve_new("escape/new.txt").is_err());
    }

    #[test]
    fn mutations_require_approval() {
        assert!(matches!(
            classify_tool("file_delete", &Value::Null),
            PolicyDecision::RequireApproval(_)
        ));
        assert_eq!(
            classify_tool("file_read", &Value::Null),
            PolicyDecision::Allow
        );
    }
}
