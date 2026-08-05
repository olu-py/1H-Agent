use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub provider: ProviderConfig,
    pub runtime: RuntimeConfig,
    pub security: SecurityConfig,
    #[serde(skip)]
    pub data_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    pub use_previous_response_id: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    ChatCompletions,
    #[default]
    Responses,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub max_agent_turns: usize,
    pub command_timeout_seconds: u64,
    pub max_tool_output_bytes: usize,
    pub max_fetch_bytes: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub allow_private_networks: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderConfig::default(),
            runtime: RuntimeConfig::default(),
            security: SecurityConfig::default(),
            data_dir: PathBuf::new(),
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Responses,
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5-mini".into(),
            use_previous_response_id: false,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_agent_turns: 8,
            command_timeout_seconds: 60,
            max_tool_output_bytes: 1024 * 1024,
            max_fetch_bytes: 10 * 1024 * 1024,
        }
    }
}

impl Config {
    pub fn load(explicit_path: Option<&Path>, workspace: &Path) -> Result<Self> {
        let path = explicit_path
            .map(Path::to_path_buf)
            .or_else(default_config_path);
        let mut config = if let Some(path) = path.filter(|path| path.exists()) {
            let value = fs::read_to_string(&path)
                .with_context(|| format!("failed to read config {}", path.display()))?;
            toml::from_str(&value).with_context(|| format!("invalid config {}", path.display()))?
        } else {
            Self::default()
        };

        if let Ok(value) = env::var("AGENT_API_BASE") {
            if !value.trim().is_empty() {
                config.provider.base_url = value;
            }
        }
        if let Ok(value) = env::var("AGENT_MODEL") {
            if !value.trim().is_empty() {
                config.provider.model = value;
            }
        }
        if let Ok(value) = env::var("AGENT_PROVIDER") {
            config.provider.kind = match value.to_ascii_lowercase().as_str() {
                "chat" | "chat_completions" => ProviderKind::ChatCompletions,
                "responses" => ProviderKind::Responses,
                _ => anyhow::bail!("AGENT_PROVIDER must be 'chat' or 'responses'"),
            };
        }

        config.provider.base_url = config.provider.base_url.trim_end_matches('/').to_owned();
        if config.provider.base_url.is_empty() || config.provider.model.trim().is_empty() {
            anyhow::bail!("provider base_url and model must not be empty");
        }
        if config.runtime.max_agent_turns == 0 || config.runtime.max_agent_turns > 32 {
            anyhow::bail!("max_agent_turns must be between 1 and 32");
        }

        config.data_dir = env::var_os("AGENT_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().map(|path| path.join("1h-agent")))
            .unwrap_or_else(|| workspace.join(".1h-agent"));
        Ok(config)
    }
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|path| path.join("1h-agent").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_bounded() {
        let config = Config::default();
        assert_eq!(config.provider.kind, ProviderKind::Responses);
        assert!(config.runtime.max_agent_turns <= 32);
        assert!(config.runtime.max_fetch_bytes >= config.runtime.max_tool_output_bytes);
    }
}
