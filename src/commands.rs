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
pub enum PaletteAction {
    Command(&'static str),
    CycleMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteItem {
    pub label: &'static str,
    pub command: Option<&'static str>,
    pub shortcut: Option<&'static str>,
    pub description: &'static str,
    pub action: PaletteAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteMatch {
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

pub const PALETTE_ITEMS: &[PaletteItem] = &[
    PaletteItem {
        label: "帮助",
        command: Some("/help"),
        shortcut: None,
        description: "显示可用命令和输入语法",
        action: PaletteAction::Command("/help"),
    },
    PaletteItem {
        label: "新建会话",
        command: Some("/new"),
        shortcut: Some("Ctrl+N"),
        description: "创建并切换到一个新会话",
        action: PaletteAction::Command("/new"),
    },
    PaletteItem {
        label: "会话列表",
        command: Some("/sessions"),
        shortcut: None,
        description: "显示当前 workspace 中的会话",
        action: PaletteAction::Command("/sessions"),
    },
    PaletteItem {
        label: "重命名会话",
        command: Some("/rename"),
        shortcut: None,
        description: "重命名当前会话；参数仍需在输入框中提供",
        action: PaletteAction::Command("/rename"),
    },
    PaletteItem {
        label: "删除会话",
        command: Some("/delete"),
        shortcut: None,
        description: "删除当前会话（至少保留一个会话）",
        action: PaletteAction::Command("/delete"),
    },
    PaletteItem {
        label: "分支当前会话",
        command: Some("/fork"),
        shortcut: None,
        description: "从当前历史创建一个新分支会话",
        action: PaletteAction::Command("/fork"),
    },
    PaletteItem {
        label: "撤销",
        command: Some("/undo"),
        shortcut: None,
        description: "将当前会话回退一轮",
        action: PaletteAction::Command("/undo"),
    },
    PaletteItem {
        label: "重做",
        command: Some("/redo"),
        shortcut: None,
        description: "恢复已撤销的一轮",
        action: PaletteAction::Command("/redo"),
    },
    PaletteItem {
        label: "压缩上下文",
        command: Some("/compact"),
        shortcut: None,
        description: "总结较早历史以释放上下文空间",
        action: PaletteAction::Command("/compact"),
    },
    PaletteItem {
        label: "恢复压缩",
        command: Some("/uncompact"),
        shortcut: None,
        description: "恢复最近一次压缩前的历史",
        action: PaletteAction::Command("/uncompact"),
    },
    PaletteItem {
        label: "导出会话",
        command: Some("/export"),
        shortcut: None,
        description: "将当前会话导出为 Markdown 文件",
        action: PaletteAction::Command("/export"),
    },
    PaletteItem {
        label: "查看改动",
        command: Some("/diff"),
        shortcut: None,
        description: "显示 workspace 中的未提交改动",
        action: PaletteAction::Command("/diff"),
    },
    PaletteItem {
        label: "当前模型",
        command: Some("/model"),
        shortcut: None,
        description: "显示当前模型；指定模型仍通过命令输入",
        action: PaletteAction::Command("/model"),
    },
    PaletteItem {
        label: "Provider 设置",
        command: Some("/provider"),
        shortcut: Some("Ctrl+S"),
        description: "打开 Provider 配置与密钥设置",
        action: PaletteAction::Command("/provider"),
    },
    PaletteItem {
        label: "当前 Agent",
        command: Some("/agent"),
        shortcut: None,
        description: "显示当前 Agent 模式；指定 Agent 仍通过命令输入",
        action: PaletteAction::Command("/agent"),
    },
    PaletteItem {
        label: "计划模式",
        command: Some("/plan"),
        shortcut: None,
        description: "切换到计划模式",
        action: PaletteAction::Command("/plan"),
    },
    PaletteItem {
        label: "构建模式",
        command: Some("/build"),
        shortcut: None,
        description: "切换到构建模式",
        action: PaletteAction::Command("/build"),
    },
    PaletteItem {
        label: "探索模式",
        command: Some("/explore"),
        shortcut: None,
        description: "切换到探索模式",
        action: PaletteAction::Command("/explore"),
    },
    PaletteItem {
        label: "集群模式",
        command: Some("/cluster"),
        shortcut: None,
        description: "切换到集群模式",
        action: PaletteAction::Command("/cluster"),
    },
    PaletteItem {
        label: "切换模式",
        command: None,
        shortcut: None,
        description: "在构建、计划、探索与集群模式间循环",
        action: PaletteAction::CycleMode,
    },
    PaletteItem {
        label: "清空显示",
        command: Some("/clear"),
        shortcut: None,
        description: "清空屏幕显示，不删除会话历史",
        action: PaletteAction::Command("/clear"),
    },
    PaletteItem {
        label: "退出",
        command: Some("/quit"),
        shortcut: Some("Ctrl+C"),
        description: "退出 1H-Agent",
        action: PaletteAction::Command("/quit"),
    },
];

pub fn matches(query: &str, limit: usize) -> Vec<PaletteMatch> {
    let mut results = PALETTE_ITEMS
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let score = [
                fuzzy_score(query, item.label),
                item.command.and_then(|command| fuzzy_score(query, command)),
            ]
            .into_iter()
            .flatten()
            .min()?;
            Some(PaletteMatch { index, score })
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
        assert_eq!(PALETTE_ITEMS[matches[0].index].command, Some("/sessions"));
    }

    #[test]
    fn palette_catalog_merges_former_leader_actions_with_commands() {
        assert_eq!(
            PALETTE_ITEMS
                .iter()
                .filter(|item| item.command == Some("/new"))
                .count(),
            1
        );
        assert!(PALETTE_ITEMS.iter().any(|item| {
            item.action == PaletteAction::CycleMode && item.description.contains("循环")
        }));
    }
}
