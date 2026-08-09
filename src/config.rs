use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::commands::AgentMode;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub provider: ProviderConfig,
    pub ui: UiConfig,
    pub runtime: RuntimeConfig,
    pub security: SecurityConfig,
    pub permissions: PermissionConfig,
    pub browser: BrowserConfig,
    pub commands: Vec<CustomCommandConfig>,
    pub agents: Vec<AgentConfig>,
    pub mcp_servers: Vec<McpServerConfig>,
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
    pub context_window_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct UiConfig {
    pub context_meter: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            context_meter: true,
        }
    }
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PermissionConfig {
    pub tools: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BrowserConfig {
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
    pub keep_alive_seconds: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::new(),
            args: Vec::new(),
            timeout_seconds: 30,
            max_output_bytes: 2 * 1024 * 1024,
            keep_alive_seconds: 30,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CustomCommandConfig {
    pub name: String,
    pub description: String,
    pub template: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub name: String,
    pub mode: AgentMode,
    pub max_turns: usize,
    pub allowed_tools: Vec<String>,
    pub system_prompt: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            mode: AgentMode::Explore,
            max_turns: 3,
            allowed_tools: Vec::new(),
            system_prompt: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub enabled: bool,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderConfig::default(),
            ui: UiConfig::default(),
            runtime: RuntimeConfig::default(),
            security: SecurityConfig::default(),
            permissions: PermissionConfig::default(),
            browser: BrowserConfig::default(),
            commands: Vec::new(),
            agents: Vec::new(),
            mcp_servers: Vec::new(),
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
            context_window_tokens: None,
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
        if let Some(limit) = config.provider.context_window_tokens {
            if limit < 4096 {
                anyhow::bail!("provider.context_window_tokens must be at least 4096");
            }
            config.provider.context_window_tokens = Some(limit.min(10_000_000));
        }
        if config.runtime.max_agent_turns == 0 || config.runtime.max_agent_turns > 32 {
            anyhow::bail!("max_agent_turns must be between 1 and 32");
        }
        if config.browser.timeout_seconds == 0 || config.browser.timeout_seconds > 3600 {
            anyhow::bail!("browser timeout must be between 1 and 3600 seconds");
        }
        config.browser.max_output_bytes = config.browser.max_output_bytes.min(8 * 1024 * 1024);
        config.browser.keep_alive_seconds = config.browser.keep_alive_seconds.min(300);
        for agent in &mut config.agents {
            agent.max_turns = agent.max_turns.clamp(1, 8);
        }
        for (tool, permission) in &config.permissions.tools {
            if !matches!(permission.as_str(), "allow" | "ask" | "deny") {
                anyhow::bail!("permission for {tool} must be allow, ask, or deny");
            }
        }
        for server in &mut config.mcp_servers {
            server.timeout_seconds = server.timeout_seconds.clamp(1, 3600);
            server.max_output_bytes = server.max_output_bytes.clamp(1024, 8 * 1024 * 1024);
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
    pub fn resolved_context_window_tokens(&self) -> Option<u64> {
        self.context_window_tokens
            .or_else(|| known_context_window(&self.model))
    }

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
            context_window_tokens: None,
        }
    }

    pub fn supports_responses(self) -> bool {
        matches!(self, Self::OpenAi | Self::DeepSeek | Self::Custom)
    }
}

fn known_context_window(model: &str) -> Option<u64> {
    let model = model.to_ascii_lowercase();
    if model.starts_with("gpt-5") {
        Some(400_000)
    } else if model.starts_with("gpt-4.1") {
        Some(1_047_576)
    } else if model.starts_with("gpt-4o")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || matches!(model.as_str(), "deepseek-chat" | "deepseek-reasoner")
    {
        Some(128_000)
    } else {
        None
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

    #[test]
    fn context_window_uses_registry_without_guessing_unknown_models() {
        let mut provider = ProviderPreset::DeepSeek.defaults();
        provider.model = "deepseek-chat".into();
        assert_eq!(provider.resolved_context_window_tokens(), Some(128_000));

        provider.model = "deepseek-v4-flash".into();
        assert_eq!(provider.resolved_context_window_tokens(), None);
    }

    #[test]
    fn explicit_context_window_override_wins() {
        let mut provider = ProviderPreset::OpenAi.defaults();
        provider.context_window_tokens = Some(32_768);
        assert_eq!(provider.resolved_context_window_tokens(), Some(32_768));
    }
}
