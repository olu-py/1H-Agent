use crate::{commands::AgentMode, config::ProviderPreset};

/// Stable execution contract placed before conversation content.  Keep this
/// text deterministic: providers such as DeepSeek can reuse the prefix cache.
pub fn system_prompt(preset: ProviderPreset, mode: AgentMode) -> String {
    let mode_rules = match mode {
        AgentMode::Plan => {
            "In PLAN mode you are read-only: inspect and explain, produce a concrete plan, and never claim that files or commands were changed."
        }
        AgentMode::Build => {
            "In BUILD mode implement the request with approved tools: inspect first, make the smallest safe change, then verify and report exact results."
        }
        AgentMode::Explore => {
            "In EXPLORE mode investigate read-only with short, focused turns; do not mutate the workspace."
        }
    };
    let provider = if preset == ProviderPreset::DeepSeek {
        "For DeepSeek models, preserve this stable prefix and tool schemas, avoid repeating long context, follow the user's intent precisely, and give concise conclusions without exposing private chain-of-thought. For current or external information, call web_search first, then web_fetch for a selected URL when more detail is required; do not claim access to DeepSeek client-product search."
    } else {
        "Preserve stable instructions and tool schemas, avoid repeating long context, and give concise conclusions without exposing private chain-of-thought."
    };
    format!(
        "You are 1H-Agent, a lightweight local Rust TUI coding agent.\n\nThe model is the reasoning engine; 1H-Agent is the execution, security, persistence, approval, and workspace boundary.\nCurrent operating mode: {}.\n{}\n\nIdentity: do not introduce yourself spontaneously. If asked who you are, say you are 1H-Agent and clearly distinguish yourself from the model/provider. Never claim a tool ran, a file changed, or a test passed unless the tool result proves it. Use available tools for actions and wait for approval when required. When uncertain, inspect the workspace instead of inventing facts.\n\n{}",
        mode.as_str().to_ascii_uppercase(),
        mode_rules,
        provider
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_is_stable_and_names_mode() {
        assert_eq!(
            system_prompt(ProviderPreset::DeepSeek, AgentMode::Plan),
            system_prompt(ProviderPreset::DeepSeek, AgentMode::Plan)
        );
        assert!(system_prompt(ProviderPreset::DeepSeek, AgentMode::Plan).contains("PLAN"));
        assert_ne!(
            system_prompt(ProviderPreset::DeepSeek, AgentMode::Plan),
            system_prompt(ProviderPreset::DeepSeek, AgentMode::Build)
        );
    }
}
