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
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub preset: ProviderPreset,
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    pub use_previous_response_id: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPreset {
    #[default]
    OpenAi,
    DeepSeek,
    Qwen,
    Volcano,
    Custom,
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
            config_path: None,
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            preset: ProviderPreset::OpenAi,
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
        let mut config = if let Some(path) = path.as_ref().filter(|path| path.exists()) {
            let value = fs::read_to_string(path)
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

        config.provider.validate()?;
        if config.runtime.max_agent_turns == 0 || config.runtime.max_agent_turns > 32 {
            anyhow::bail!("max_agent_turns must be between 1 and 32");
        }

        config.data_dir = env::var_os("AGENT_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().map(|path| path.join("1h-agent")))
            .unwrap_or_else(|| workspace.join(".1h-agent"));
        config.config_path = path;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = self
            .config_path
            .as_ref()
            .context("no writable configuration directory is available")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        let value = toml::to_string_pretty(self).context("failed to serialize configuration")?;
        fs::write(path, value).with_context(|| format!("failed to save config {}", path.display()))
    }
}

impl ProviderConfig {
    pub fn validate(&mut self) -> Result<()> {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_owned();
        self.model = self.model.trim().to_owned();
        if self.base_url.contains('{') || self.base_url.contains('}') {
            anyhow::bail!("replace placeholders in the provider Base URL");
        }
        let url = url::Url::parse(&self.base_url).context("provider Base URL is invalid")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            anyhow::bail!("provider Base URL must be HTTP or HTTPS with a host");
        }
        if !url.username().is_empty() || url.password().is_some() {
            anyhow::bail!("provider Base URL must not contain credentials");
        }
        if self.model.is_empty() {
            anyhow::bail!("model must not be empty");
        }
        if !self.preset.supports_responses() {
            self.kind = ProviderKind::ChatCompletions;
            self.use_previous_response_id = false;
        }
        Ok(())
    }
}

impl ProviderPreset {
    pub const ALL: [Self; 5] = [
        Self::OpenAi,
        Self::DeepSeek,
        Self::Qwen,
        Self::Volcano,
        Self::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::DeepSeek => "DeepSeek",
            Self::Qwen => "Qwen / Bailian",
            Self::Volcano => "Volcano Ark",
            Self::Custom => "Custom compatible",
        }
    }

    pub fn key_id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::DeepSeek => "deepseek",
            Self::Qwen => "qwen",
            Self::Volcano => "volcano",
            Self::Custom => "custom",
        }
    }

    pub fn defaults(self) -> ProviderConfig {
        let (kind, base_url, model) = match self {
            Self::OpenAi => (
                ProviderKind::Responses,
                "https://api.openai.com/v1",
                "gpt-5-mini",
            ),
            Self::DeepSeek => (
                ProviderKind::ChatCompletions,
                "https://api.deepseek.com",
                "deepseek-v4-flash",
            ),
            Self::Qwen => (
                ProviderKind::ChatCompletions,
                "https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
                "qwen3.8-max",
            ),
            Self::Volcano => (
                ProviderKind::ChatCompletions,
                "https://ark.cn-beijing.volces.com/api/v3",
                "doubao-seed-2-1-pro-260628",
            ),
            Self::Custom => (
                ProviderKind::ChatCompletions,
                "https://api.example.com/v1",
                "model-name",
            ),
        };
        ProviderConfig {
            preset: self,
            kind,
            base_url: base_url.into(),
            model: model.into(),
            use_previous_response_id: false,
        }
    }

    pub fn supports_responses(self) -> bool {
        matches!(self, Self::OpenAi | Self::DeepSeek | Self::Custom)
    }
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "Chat Completions",
            Self::Responses => "Responses",
        }
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

    #[test]
    fn presets_have_expected_protocols_and_current_models() {
        let deepseek = ProviderPreset::DeepSeek.defaults();
        assert_eq!(deepseek.model, "deepseek-v4-flash");
        assert_eq!(deepseek.base_url, "https://api.deepseek.com");
        let qwen = ProviderPreset::Qwen.defaults();
        assert!(qwen.base_url.contains("{WorkspaceId}"));
        assert_eq!(qwen.kind, ProviderKind::ChatCompletions);
        let volcano = ProviderPreset::Volcano.defaults();
        assert!(volcano.base_url.ends_with("/api/v3"));
    }

    #[test]
    fn qwen_requires_workspace_id_before_saving() {
        let mut qwen = ProviderPreset::Qwen.defaults();
        assert!(qwen.validate().is_err());
        qwen.base_url = qwen.base_url.replace("{WorkspaceId}", "ws-example");
        assert!(qwen.validate().is_ok());
    }
}
