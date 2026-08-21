use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::UNIX_EPOCH,
};

use ignore::WalkBuilder;
use regex::Regex;
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
    line_numbers: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    path: String,
    query: String,
    max_results: Option<usize>,
    regex: Option<bool>,
    ignore_case: Option<bool>,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    path: String,
    pattern: String,
    max_results: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutlineArgs {
    path: String,
    max_results: Option<usize>,
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
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::Execution("file is not valid UTF-8".into()))?;
    if args.line_numbers.unwrap_or(false) {
        let numbered = text
            .lines()
            .enumerate()
            .map(|(index, line)| format!("{:>4}\t{line}", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(numbered)
    } else {
        Ok(text)
    }
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
    let regex_enabled = args.regex.unwrap_or(false);
    let ignore_case = args.ignore_case.unwrap_or(false);
    let matcher: SearchMatcher = if regex_enabled {
        let pattern = if ignore_case {
            format!("(?i){}", args.query)
        } else {
            args.query.clone()
        };
        let regex = Regex::new(&pattern).map_err(|error| {
            ToolError::InvalidArguments(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid regex: {error}"),
            )))
        })?;
        SearchMatcher::Regex(regex)
    } else if ignore_case {
        SearchMatcher::ContainsCaseInsensitive(args.query)
    } else {
        SearchMatcher::Contains(args.query)
    };
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
            if matcher.matches(line) {
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

/// Line matcher for `file_search`, unifying the substring fast path with the
/// case-insensitive and regex variants.
enum SearchMatcher {
    Contains(String),
    ContainsCaseInsensitive(String),
    Regex(Regex),
}

impl SearchMatcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            Self::Contains(query) => line.contains(query.as_str()),
            Self::ContainsCaseInsensitive(query) => line
                .to_ascii_lowercase()
                .contains(&query.to_ascii_lowercase()),
            Self::Regex(regex) => regex.is_match(line),
        }
    }
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

pub fn glob(
    workspace: &Workspace,
    value: &Value,
    output_limit: usize,
) -> Result<String, ToolError> {
    let args: GlobArgs = serde_json::from_value(value.clone())?;
    if args.pattern.is_empty() {
        return Err(ToolError::Execution("pattern must not be empty".into()));
    }
    let root = workspace
        .resolve_existing(args.path)
        .map_err(security_error)?;
    let matcher = globset::GlobBuilder::new(&args.pattern)
        .literal_separator(false)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| {
            ToolError::InvalidArguments(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid glob pattern: {error}"),
            )))
        })?;
    let max_results = args.max_results.unwrap_or(200).min(1000);
    let mut entries = Vec::new();
    let mut encoded_size = 0usize;

    for entry in WalkBuilder::new(root)
        .hidden(false)
        .max_depth(Some(16))
        .build()
        .filter_map(Result::ok)
    {
        if entries.len() >= max_results || encoded_size >= output_limit {
            break;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let file_name = entry.file_name().to_string_lossy();
        if !matcher.is_match(file_name.as_ref()) {
            continue;
        }
        let item = json!({
            "path": display_relative(workspace, entry.path()),
            "type": if metadata.is_dir() { "directory" } else if metadata.is_file() { "file" } else { "other" },
            "bytes": metadata.len(),
        });
        encoded_size += item.to_string().len();
        entries.push(item);
    }
    serde_json::to_string_pretty(&entries).map_err(ToolError::from)
}

/// Line-start heuristic outline (repo map): extracts symbols like `fn`,
/// `struct`, `impl`, `trait`, `enum`, `mod`, `class`, `def`, `function`, `func`
/// with their 1-based line numbers. Pure regex; intentionally no tree-sitter or
/// LSP.
pub fn repo_map(
    workspace: &Workspace,
    value: &Value,
    output_limit: usize,
) -> Result<String, ToolError> {
    let args: OutlineArgs = serde_json::from_value(value.clone())?;
    let root = workspace
        .resolve_existing(args.path)
        .map_err(security_error)?;
    let max_results = args.max_results.unwrap_or(500).min(2000);
    let mut symbols = Vec::new();
    let mut encoded_size = 0usize;

    for entry in WalkBuilder::new(root)
        .hidden(false)
        .max_depth(Some(16))
        .build()
        .filter_map(Result::ok)
    {
        if symbols.len() >= max_results || encoded_size >= output_limit {
            break;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            continue;
        }
        let Ok(content) = fs::read(entry.path()) else {
            continue;
        };
        // Skip binary files quickly: NUL byte in the first 1 KiB.
        if content.get(..1024).is_some_and(|head| head.contains(&0)) {
            continue;
        }
        let Ok(text) = String::from_utf8(content) else {
            continue;
        };
        let relative = display_relative(workspace, entry.path());
        for (index, line) in text.lines().enumerate() {
            let Some(kind) = outline_kind(line) else {
                continue;
            };
            let symbol = line.trim();
            let symbol = if symbol.len() > 120 {
                let end = char_boundary(symbol, 120);
                &symbol[..end]
            } else {
                symbol
            };
            let item = json!({
                "path": relative,
                "line": index + 1,
                "symbol": symbol,
                "kind": kind,
            });
            encoded_size += item.to_string().len();
            symbols.push(item);
            if symbols.len() >= max_results || encoded_size >= output_limit {
                break;
            }
        }
    }
    symbols.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    serde_json::to_string_pretty(&symbols).map_err(ToolError::from)
}

/// Detects a line-start symbol declaration and returns its kind, or `None`.
fn outline_kind(line: &str) -> Option<&'static str> {
    let line = line.trim_start();
    for (prefix, kind) in [
        ("fn ", "fn"),
        ("pub fn ", "fn"),
        ("struct ", "struct"),
        ("impl ", "impl"),
        ("trait ", "trait"),
        ("enum ", "enum"),
        ("mod ", "mod"),
        ("class ", "class"),
        ("def ", "def"),
        ("function ", "function"),
        ("func ", "func"),
    ] {
        if line.starts_with(prefix) {
            return Some(kind);
        }
    }
    None
}

/// Largest byte index at or below `limit` that is a UTF-8 char boundary. Avoids
/// depending on the recent `floor_char_boundary` stable API so the MSRV (1.85)
/// is preserved.
fn char_boundary(value: &str, limit: usize) -> usize {
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
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

    #[test]
    fn search_supports_regex_ignore_case_and_falls_back_to_substring() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(
            &workspace,
            &json!({"path":"a.txt","content":"Foo bar\nfoo baz"}),
        )
        .unwrap();
        // Substring fast path (default).
        let sub = search(&workspace, &json!({"path":".","query":"bar"}), 4096).unwrap();
        assert!(sub.contains("Foo bar"));
        // ignore_case matches both lines.
        let ci = search(
            &workspace,
            &json!({"path":".","query":"foo","ignore_case":true}),
            4096,
        )
        .unwrap();
        assert!(ci.contains("Foo bar"));
        assert!(ci.contains("foo baz"));
        // regex matches only "baz" (bar does not end in 'z').
        let regex = search(
            &workspace,
            &json!({"path":".","query":"ba[z]$","regex":true}),
            4096,
        )
        .unwrap();
        assert!(regex.contains("foo baz"));
        assert!(!regex.contains("Foo bar"));
    }

    #[test]
    fn search_reports_invalid_regex_as_invalid_arguments() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(&workspace, &json!({"path":"a.txt","content":"x"})).unwrap();
        let error = search(
            &workspace,
            &json!({"path":".","query":"[unclosed","regex":true}),
            4096,
        )
        .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn glob_finds_files_by_pattern() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(&workspace, &json!({"path":"a.rs","content":"fn a() {}"})).unwrap();
        write(&workspace, &json!({"path":"b.ts","content":"let b = 1;"})).unwrap();
        std::fs::create_dir_all(root.path().join("sub")).unwrap();
        write(
            &workspace,
            &json!({"path":"sub/c.rs","content":"fn c() {}"}),
        )
        .unwrap();
        let rs = glob(&workspace, &json!({"path":".","pattern":"*.rs"}), 4096).unwrap();
        assert!(rs.contains("a.rs"));
        assert!(!rs.contains("b.ts"));
        assert!(rs.contains("c.rs"));
    }

    #[test]
    fn repo_map_extracts_symbols_and_skips_binary() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(
            &workspace,
            &json!({"path":"lib.rs","content":"use std;\n\npub fn run() {}\nstruct Config {}\nimpl Config {}\n"}),
        )
        .unwrap();
        std::fs::write(root.path().join("blob.bin"), vec![0u8, 1, 2, 3]).unwrap();
        let outline = repo_map(&workspace, &json!({"path":"."}), 4096).unwrap();
        assert!(outline.contains("\"fn\""));
        assert!(outline.contains("pub fn run()"));
        assert!(outline.contains("\"struct\""));
        assert!(outline.contains("\"impl\""));
        assert!(!outline.contains("blob.bin"));
        // Line numbers are 1-based and present.
        assert!(outline.contains("\"line\": 3"));
    }

    #[test]
    fn read_supports_line_numbers() {
        let root = tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        write(
            &workspace,
            &json!({"path":"a.txt","content":"one\ntwo\nthree"}),
        )
        .unwrap();
        let numbered = read(
            &workspace,
            &json!({"path":"a.txt","line_numbers":true}),
            100,
        )
        .unwrap();
        assert!(numbered.contains("\tone"));
        assert!(numbered.contains("1\tone"));
        assert!(numbered.contains("2\ttwo"));
        assert!(numbered.contains("3\tthree"));
        // Plain read unchanged.
        let plain = read(&workspace, &json!({"path":"a.txt"}), 100).unwrap();
        assert_eq!(plain, "one\ntwo\nthree");
    }
}
