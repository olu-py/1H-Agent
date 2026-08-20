use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::UNIX_EPOCH,
};

use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use super::ToolError;
use crate::security::Workspace;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    max_bytes: Option<usize>,
    offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    path: String,
    query: String,
    max_results: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferArgs {
    source: String,
    destination: String,
}

pub fn list(workspace: &Workspace, value: &Value) -> Result<String, ToolError> {
    let args: PathArgs = serde_json::from_value(value.clone())?;
    let path = workspace
        .resolve_existing(args.path)
        .map_err(security_error)?;
    let mut entries = fs::read_dir(path)
        .map_err(execution_error)?
        .map(|entry| {
            let entry = entry.map_err(execution_error)?;
            let metadata = entry.metadata().map_err(execution_error)?;
            Ok(json!({
                "name": entry.file_name().to_string_lossy(),
                "type": if metadata.is_dir() { "directory" } else if metadata.is_file() { "file" } else { "other" },
                "bytes": metadata.len(),
            }))
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    serde_json::to_string_pretty(&entries).map_err(ToolError::from)
}

pub fn stat(workspace: &Workspace, value: &Value) -> Result<String, ToolError> {
    let args: PathArgs = serde_json::from_value(value.clone())?;
    let path = workspace
        .resolve_existing(args.path)
        .map_err(security_error)?;
    let metadata = fs::metadata(&path).map_err(execution_error)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    Ok(json!({
        "path": display_relative(workspace, &path),
        "type": if metadata.is_dir() { "directory" } else if metadata.is_file() { "file" } else { "other" },
        "bytes": metadata.len(),
        "readonly": metadata.permissions().readonly(),
        "modified_unix": modified,
    })
    .to_string())
}

pub fn read(workspace: &Workspace, value: &Value, limit: usize) -> Result<String, ToolError> {
    let args: ReadArgs = serde_json::from_value(value.clone())?;
    let path = workspace
        .resolve_existing(args.path)
        .map_err(security_error)?;
    let limit = args.max_bytes.unwrap_or(limit).min(limit);
    let mut file = fs::File::open(path).map_err(execution_error)?;
    if let Some(offset) = args.offset {
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(execution_error)?;
    }
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(execution_error)?;
    if bytes.len() > limit {
        bytes.truncate(limit);
        bytes.extend_from_slice(b"\n[output truncated]");
    }
    String::from_utf8(bytes).map_err(|_| ToolError::Execution("file is not valid UTF-8".into()))
}

pub fn search(
    workspace: &Workspace,
    value: &Value,
    output_limit: usize,
) -> Result<String, ToolError> {
    let args: SearchArgs = serde_json::from_value(value.clone())?;
    if args.query.is_empty() {
        return Err(ToolError::Execution(
            "search query must not be empty".into(),
        ));
    }
    let root = workspace
        .resolve_existing(args.path)
        .map_err(security_error)?;
    let max_results = args.max_results.unwrap_or(200).min(1000);
    let mut matches = Vec::new();
    let mut encoded_size = 0usize;

    for entry in WalkBuilder::new(root)
        .hidden(false)
        .build()
        .filter_map(Result::ok)
    {
        if matches.len() >= max_results || encoded_size >= output_limit {
            break;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for (index, line) in content.lines().enumerate() {
            if line.contains(&args.query) {
                let item = json!({
                    "path": display_relative(workspace, entry.path()),
                    "line": index + 1,
                    "text": line,
                });
                encoded_size += item.to_string().len();
                matches.push(item);
                if matches.len() >= max_results || encoded_size >= output_limit {
                    break;
                }
            }
        }
    }
    serde_json::to_string_pretty(&matches).map_err(ToolError::from)
}

pub fn mkdir(workspace: &Workspace, value: &Value) -> Result<String, ToolError> {
    let args: PathArgs = serde_json::from_value(value.clone())?;
    let path = workspace.resolve_new(args.path).map_err(security_error)?;
    fs::create_dir_all(&path).map_err(execution_error)?;
    Ok(format!("created {}", display_relative(workspace, &path)))
}

pub fn write(workspace: &Workspace, value: &Value) -> Result<String, ToolError> {
    let args: WriteArgs = serde_json::from_value(value.clone())?;
    let path = workspace.resolve_new(args.path).map_err(security_error)?;
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::Execution("destination has no parent".into()))?;
    workspace.resolve_existing(parent).map_err(security_error)?;
    fs::write(&path, args.content.as_bytes()).map_err(execution_error)?;
    Ok(format!(
        "wrote {} bytes to {}",
        args.content.len(),
        display_relative(workspace, &path)
    ))
}

pub fn edit(workspace: &Workspace, value: &Value) -> Result<String, ToolError> {
    let args: EditArgs = serde_json::from_value(value.clone())?;
    let path = workspace
        .resolve_existing(args.path)
        .map_err(security_error)?;
    if args.old_string.is_empty() {
        return Err(ToolError::Execution("old_string must not be empty".into()));
    }
    let content = fs::read_to_string(&path)
        .map_err(|_| ToolError::Execution("file is not valid UTF-8".into()))?;
    let count = content.matches(&args.old_string).count();
    if count == 0 {
        return Err(ToolError::Execution(
            "no match found; read the file first to confirm its content".into(),
        ));
    }
    if count > 1 && args.replace_all != Some(true) {
        return Err(ToolError::Execution(format!(
            "old_string matched {count} times; widen its context or set replace_all=true"
        )));
    }
    let replacement_count = if args.replace_all == Some(true) {
        content.matches(&args.old_string).count()
    } else {
        1
    };
    let updated = content.replace(&args.old_string, &args.new_string);
    fs::write(&path, updated.as_bytes()).map_err(execution_error)?;
    Ok(format!(
        "edited {}: {replacement_count} replacement(s)",
        display_relative(workspace, &path)
    ))
}

pub fn copy(workspace: &Workspace, value: &Value) -> Result<String, ToolError> {
    let args: TransferArgs = serde_json::from_value(value.clone())?;
    let source = workspace
        .resolve_existing(args.source)
        .map_err(security_error)?;
    let destination = workspace
        .resolve_new(args.destination)
        .map_err(security_error)?;
    let bytes = fs::copy(&source, &destination).map_err(execution_error)?;
    Ok(format!(
        "copied {bytes} bytes to {}",
        display_relative(workspace, &destination)
    ))
}

pub fn move_path(workspace: &Workspace, value: &Value) -> Result<String, ToolError> {
    let args: TransferArgs = serde_json::from_value(value.clone())?;
    let source = workspace
        .resolve_existing(args.source)
        .map_err(security_error)?;
    let destination = workspace
        .resolve_new(args.destination)
        .map_err(security_error)?;
    fs::rename(&source, &destination).map_err(execution_error)?;
    Ok(format!(
        "moved to {}",
        display_relative(workspace, &destination)
    ))
}

pub fn delete(workspace: &Workspace, value: &Value) -> Result<String, ToolError> {
    let args: PathArgs = serde_json::from_value(value.clone())?;
    let path = workspace
        .resolve_existing(args.path)
        .map_err(security_error)?;
    if path == workspace.root() {
        return Err(ToolError::Security(
            "workspace root cannot be deleted".into(),
        ));
    }
    if path.is_dir() {
        fs::remove_dir(&path).map_err(execution_error)?;
    } else {
        fs::remove_file(&path).map_err(execution_error)?;
    }
    Ok(format!("deleted {}", display_relative(workspace, &path)))
}

fn display_relative(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn security_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Security(error.to_string())
}

fn execution_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_reads_and_searches_inside_workspace() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(&workspace, &json!({"path":"a.txt","content":"one\ntwo"})).unwrap();
        assert_eq!(
            read(&workspace, &json!({"path":"a.txt"}), 100).unwrap(),
            "one\ntwo"
        );
        let result = search(&workspace, &json!({"path":".","query":"two"}), 4096).unwrap();
        assert!(result.contains("a.txt"));
    }

    #[test]
    fn read_supports_byte_offset() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let content = "0123456789";
        write(&workspace, &json!({"path":"b.txt","content": content})).unwrap();
        assert_eq!(
            read(&workspace, &json!({"path":"b.txt","offset":4}), 100).unwrap(),
            "456789"
        );
        assert_eq!(
            read(
                &workspace,
                &json!({"path":"b.txt","offset":4,"max_bytes":2}),
                100
            )
            .unwrap(),
            "45\n[output truncated]"
        );
    }

    #[test]
    fn edit_replaces_unique_match_and_persists() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(
            &workspace,
            &json!({"path":"a.txt","content":"line one\nline two\nline three"}),
        )
        .unwrap();
        let result = edit(
            &workspace,
            &json!({"path":"a.txt","old_string":"line two","new_string":"line 2"}),
        )
        .unwrap();
        assert_eq!(result, "edited a.txt: 1 replacement(s)");
        assert_eq!(
            read(&workspace, &json!({"path":"a.txt"}), 100).unwrap(),
            "line one\nline 2\nline three"
        );
    }

    #[test]
    fn edit_rejects_zero_matches() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(&workspace, &json!({"path":"a.txt","content":"line one"})).unwrap();
        let error = edit(
            &workspace,
            &json!({"path":"a.txt","old_string":"missing","new_string":"x"}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no match found"));
        assert_eq!(
            read(&workspace, &json!({"path":"a.txt"}), 100).unwrap(),
            "line one"
        );
    }

    #[test]
    fn edit_requires_unique_match_unless_replace_all() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(
            &workspace,
            &json!({"path":"a.txt","content":"one two one three"}),
        )
        .unwrap();
        let error = edit(
            &workspace,
            &json!({"path":"a.txt","old_string":"one","new_string":"1"}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("2 times"));

        let result = edit(
            &workspace,
            &json!({"path":"a.txt","old_string":"one","new_string":"1","replace_all":true}),
        )
        .unwrap();
        assert_eq!(result, "edited a.txt: 2 replacement(s)");
        assert_eq!(
            read(&workspace, &json!({"path":"a.txt"}), 100).unwrap(),
            "1 two 1 three"
        );
    }

    #[test]
    fn edit_rejects_empty_old_string() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(&workspace, &json!({"path":"a.txt","content":"content"})).unwrap();
        let error = edit(
            &workspace,
            &json!({"path":"a.txt","old_string":"","new_string":"x"}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn edit_rejects_path_escape() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(&workspace, &json!({"path":"a.txt","content":"content"})).unwrap();
        let error = edit(
            &workspace,
            &json!({"path":"../outside.txt","old_string":"x","new_string":"y"}),
        )
        .unwrap_err();
        assert!(matches!(error, ToolError::Security(_)));
    }

    #[test]
    fn edit_requires_existing_file() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let error = edit(
            &workspace,
            &json!({"path":"missing.txt","old_string":"x","new_string":"y"}),
        )
        .unwrap_err();
        assert!(matches!(error, ToolError::Security(_)));
    }
}
