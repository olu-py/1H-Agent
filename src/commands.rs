use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    NewSession,
    Sessions,
    Rename(Option<String>),
    Delete,
    Fork,
    Undo,
    Redo,
    Compact(Option<String>),
    Uncompact,
    Export(Option<String>),
    Diff,
    Model(Option<String>),
    Provider,
    Agent(Option<String>),
    Mode(AgentMode),
    Clear,
    Quit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    #[default]
    Build,
    Plan,
    Explore,
    Cluster,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Plan => "plan",
            Self::Explore => "explore",
            Self::Cluster => "cluster",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "build" => Some(Self::Build),
            "plan" => Some(Self::Plan),
            "explore" => Some(Self::Explore),
            "cluster" => Some(Self::Cluster),
            _ => None,
        }
    }
}

impl fmt::Display for AgentMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub fn parse(input: &str) -> Option<Command> {
    let mut parts = input.trim().splitn(2, char::is_whitespace);
    let name = parts.next()?.strip_prefix('/')?.to_ascii_lowercase();
    let argument = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Some(match name.as_str() {
        "help" | "h" => Command::Help,
        "new" | "n" => Command::NewSession,
        "sessions" | "session" | "ls" => Command::Sessions,
        "rename" => Command::Rename(argument.map(str::to_owned)),
        "delete" | "rm" => Command::Delete,
        "fork" => Command::Fork,
        "undo" => Command::Undo,
        "redo" => Command::Redo,
        "compact" | "summarize" => Command::Compact(argument.map(str::to_owned)),
        "uncompact" | "decompact" => Command::Uncompact,
        "export" => Command::Export(argument.map(str::to_owned)),
        "diff" => Command::Diff,
        "model" => Command::Model(argument.map(str::to_owned)),
        "provider" => Command::Provider,
        "agent" => Command::Agent(argument.map(str::to_owned)),
        "plan" => Command::Mode(AgentMode::Plan),
        "build" => Command::Mode(AgentMode::Build),
        "explore" => Command::Mode(AgentMode::Explore),
        "cluster" => Command::Mode(AgentMode::Cluster),
        "clear" => Command::Clear,
        "quit" | "exit" | "q" => Command::Quit,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandMatch {
    pub index: usize,
    pub score: usize,
}

/// Allocation-free enough for a small static command list.  A candidate is a
/// match when the query appears as an ordered subsequence of its name.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_ascii_lowercase();
    let mut position = 0;
    let mut score = 0;
    let mut previous = None;
    for character in query.chars() {
        let offset = candidate[position..].find(character)?;
        let found = position + offset;
        score += found.saturating_sub(previous.unwrap_or(0));
        if found == 0 || candidate.as_bytes().get(found.saturating_sub(1)) == Some(&b' ') {
            score = score.saturating_sub(2);
        }
        previous = Some(found);
        position = found + character.len_utf8();
    }
    Some(score + candidate.len().saturating_sub(query.len()))
}

pub const COMMAND_NAMES: &[&str] = &[
    "/help",
    "/new",
    "/sessions",
    "/rename",
    "/delete",
    "/fork",
    "/undo",
    "/redo",
    "/compact",
    "/uncompact",
    "/export",
    "/diff",
    "/model",
    "/provider",
    "/agent",
    "/plan",
    "/build",
    "/explore",
    "/cluster",
    "/clear",
    "/quit",
];

pub fn matches(query: &str, limit: usize) -> Vec<CommandMatch> {
    let mut results = COMMAND_NAMES
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            fuzzy_score(query, candidate).map(|score| CommandMatch { index, score })
        })
        .collect::<Vec<_>>();
    results.sort_by_key(|item| (item.score, item.index));
    results.truncate(limit.min(10));
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_commands() {
        assert_eq!(parse("/new"), Some(Command::NewSession));
        assert_eq!(
            parse("/rename my session"),
            Some(Command::Rename(Some("my session".into())))
        );
        assert_eq!(parse("/plan"), Some(Command::Mode(AgentMode::Plan)));
        assert_eq!(parse("/build"), Some(Command::Mode(AgentMode::Build)));
        assert_eq!(parse("/explore"), Some(Command::Mode(AgentMode::Explore)));
        assert_eq!(parse("/missing"), None);
    }

    #[test]
    fn fuzzy_matching_is_bounded() {
        let matches = matches("ses", 100);
        assert!(!matches.is_empty());
        assert!(matches.len() <= 10);
        assert_eq!(COMMAND_NAMES[matches[0].index], "/sessions");
    }
}
