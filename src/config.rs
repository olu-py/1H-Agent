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
    /// Saved connection profiles. Each preset has at most one profile; API
    /// keys deliberately remain in the system keyring rather than TOML.
    pub providers: Vec<ProviderConfig>,
    /// Distinguishes an old config with no `providers` field from a user who
    /// intentionally removed every saved connection in the new UI.
    pub provider_profiles_initialized: bool,
    pub ui: UiConfig,
    pub runtime: RuntimeConfig,
    pub compaction: CompactionConfig,
    pub security: SecurityConfig,
    pub permissions: PermissionConfig,
    pub browser: BrowserConfig,
    pub cluster: ClusterConfig,
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
pub struct CompactionConfig {
    pub enabled: bool,
    pub auto_threshold: f32,
    pub target_ratio: f32,
    pub preserve_recent_tokens: Option<u64>,
    pub max_summary_bytes: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_threshold: 0.80,
            target_ratio: 0.55,
            preserve_recent_tokens: None,
            max_summary_bytes: 65_536,
        }
    }
}

impl CompactionConfig {
    pub fn normalize(&mut self) {
        self.auto_threshold = self.auto_threshold.clamp(0.60, 0.90);
        self.target_ratio = self.target_ratio.clamp(0.30, 0.70);
        self.max_summary_bytes = self.max_summary_bytes.clamp(4 * 1024, 256 * 1024);
        if let Some(value) = self.preserve_recent_tokens.as_mut() {
            *value = (*value).clamp(4_000, 16_000);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub preset: ProviderPreset,
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    pub use_previous_response_id: bool,
    pub native_web_search: NativeWebSearch,
    pub context_window_tokens: Option<u64>,
    pub thinking: ThinkingCapability,
    pub thinking_level: ThinkingLevel,
    pub thinking_budget_tokens: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    #[default]
    Auto,
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Enabled,
}

impl ThinkingLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Enabled => "开启",
        }
    }

    pub fn menu_label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::None => "关闭",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Enabled => "开启",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingProfileKind {
    OpenAi,
    Qwen38,
    Qwen37,
    DeepSeekPro,
    DeepSeekFlash,
    Volcano,
    Compatible,
}

#[derive(Clone, Copy, Debug)]
pub struct ThinkingProfile {
    pub options: &'static [ThinkingLevel],
    pub default: ThinkingLevel,
    pub kind: ThinkingProfileKind,
}

const OPENAI_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Auto,
    ThinkingLevel::None,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];
const QWEN38_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::None,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::XHigh,
];
const QWEN37_LEVELS: &[ThinkingLevel] = &[ThinkingLevel::None, ThinkingLevel::Enabled];
const DEEPSEEK_PRO_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Low,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];
const DEEPSEEK_FLASH_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Low,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];
const VOLCANO_LEVELS: &[ThinkingLevel] = &[ThinkingLevel::High];
const COMPATIBLE_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel::Auto,
    ThinkingLevel::None,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Max,
];

pub fn thinking_profile(preset: ProviderPreset, model: &str) -> ThinkingProfile {
    let model = model
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if preset == ProviderPreset::Qwen && model.contains("qwen38") {
        ThinkingProfile {
            options: QWEN38_LEVELS,
            default: ThinkingLevel::XHigh,
            kind: ThinkingProfileKind::Qwen38,
        }
    } else if preset == ProviderPreset::Qwen && model.contains("qwen37") {
        ThinkingProfile {
            options: QWEN37_LEVELS,
            default: ThinkingLevel::Enabled,
            kind: ThinkingProfileKind::Qwen37,
        }
    } else if preset == ProviderPreset::DeepSeek && model.contains("v4pro") {
        ThinkingProfile {
            options: DEEPSEEK_PRO_LEVELS,
            default: ThinkingLevel::High,
            kind: ThinkingProfileKind::DeepSeekPro,
        }
    } else if preset == ProviderPreset::DeepSeek && model.contains("v4flash") {
        ThinkingProfile {
            options: DEEPSEEK_FLASH_LEVELS,
            default: ThinkingLevel::High,
            kind: ThinkingProfileKind::DeepSeekFlash,
        }
    } else if preset == ProviderPreset::Volcano {
        ThinkingProfile {
            options: VOLCANO_LEVELS,
            default: ThinkingLevel::High,
            kind: ThinkingProfileKind::Volcano,
        }
    } else if preset == ProviderPreset::OpenAi {
        ThinkingProfile {
            options: OPENAI_LEVELS,
            default: ThinkingLevel::Auto,
            kind: ThinkingProfileKind::OpenAi,
        }
    } else {
        ThinkingProfile {
            options: COMPATIBLE_LEVELS,
            default: ThinkingLevel::Auto,
            kind: ThinkingProfileKind::Compatible,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingCapability {
    #[default]
    Auto,
    OpenAi,
    DeepSeek,
    Qwen,
    Volcano,
    Compatible,
    Disabled,
}

impl ThinkingCapability {
    pub const ALL: [Self; 7] = [
        Self::Auto,
        Self::OpenAi,
        Self::DeepSeek,
        Self::Qwen,
        Self::Volcano,
        Self::Compatible,
        Self::Disabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::OpenAi => "OpenAI 摘要",
            Self::DeepSeek => "DeepSeek",
            Self::Qwen => "Qwen",
            Self::Volcano => "火山方舟",
            Self::Compatible => "兼容解析",
            Self::Disabled => "关闭",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NativeWebSearch {
    #[default]
    Auto,
    Enabled,
    Disabled,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
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
    pub command_timeout_seconds: u64,
    pub max_tool_output_bytes: usize,
    pub max_fetch_bytes: usize,
    /// Hard cap on runtimes parked in the background (the active session is
    /// not counted). Overflow prefers the least-recently-parked idle runtime,
    /// then shuts down the oldest busy runtime if necessary.
    pub max_background_sessions: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ClusterConfig {
    /// Maximum active children. Missing values use the safe default of four.
    pub max_parallel_children: Option<usize>,
    /// Reserved count limits; parallel execution is currently bounded by
    /// `max_parallel_children`.
    pub max_children_per_turn: Option<usize>,
    pub max_children_per_session: Option<usize>,
    /// Active model/tool time available to one child. Queueing and approval
    /// waits are excluded from this budget.
    pub child_active_timeout_seconds: u64,
    /// Bounds applied to each child agent's working context and tool output.
    /// These are enforced in `agent::run_child`.
    pub child_max_output_bytes: usize,
    pub child_max_tool_output_bytes: usize,
    pub child_max_context_items: usize,
    pub child_max_context_bytes: usize,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            max_parallel_children: Some(4),
            max_children_per_turn: None,
            max_children_per_session: None,
            child_active_timeout_seconds: 300,
            child_max_output_bytes: 256 * 1024,
            child_max_tool_output_bytes: 128 * 1024,
            child_max_context_items: 48,
            child_max_context_bytes: 512 * 1024,
        }
    }
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
    /// Optional hard turn limit. Zero means unlimited; the child active
    /// execution budget remains the production safety bound.
    pub max_turns: usize,
    pub allowed_tools: Vec<String>,
    pub system_prompt: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            mode: AgentMode::Explore,
            max_turns: 0,
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
            providers: Vec::new(),
            provider_profiles_initialized: false,
            ui: UiConfig::default(),
            runtime: RuntimeConfig::default(),
            compaction: CompactionConfig::default(),
            security: SecurityConfig::default(),
            permissions: PermissionConfig::default(),
            browser: BrowserConfig::default(),
            cluster: ClusterConfig::default(),
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
            native_web_search: NativeWebSearch::Auto,
            context_window_tokens: None,
            thinking: ThinkingCapability::Auto,
            thinking_level: ThinkingLevel::Auto,
            thinking_budget_tokens: None,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            command_timeout_seconds: 60,
            max_tool_output_bytes: 1024 * 1024,
            max_fetch_bytes: 10 * 1024 * 1024,
            max_background_sessions: 8,
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

        // A config written by older versions has only `[provider]`. Preserve
        // that complete profile before applying process-only environment
        // overrides, so migration never loses the user's connection details.
        config.ensure_provider_profiles();
        config.compaction.normalize();

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
        config.provider.normalize_thinking();
        if let Some(limit) = config.provider.context_window_tokens {
            if limit < 4096 {
                anyhow::bail!("provider.context_window_tokens must be at least 4096");
            }
            config.provider.context_window_tokens = Some(limit.min(10_000_000));
        }
        if config.browser.timeout_seconds == 0 || config.browser.timeout_seconds > 3600 {
            anyhow::bail!("browser timeout must be between 1 and 3600 seconds");
        }
        config.browser.max_output_bytes = config.browser.max_output_bytes.min(8 * 1024 * 1024);
        config.browser.keep_alive_seconds = config.browser.keep_alive_seconds.min(300);
        config.runtime.max_background_sessions =
            config.runtime.max_background_sessions.clamp(2, 64);
        config.cluster.child_max_output_bytes = config
            .cluster
            .child_max_output_bytes
            .clamp(16 * 1024, 1024 * 1024);
        config.cluster.max_parallel_children = Some(
            config
                .cluster
                .max_parallel_children
                .unwrap_or(4)
                .clamp(1, 32),
        );
        config.cluster.child_active_timeout_seconds =
            config.cluster.child_active_timeout_seconds.clamp(30, 3600);
        config.cluster.child_max_tool_output_bytes = config
            .cluster
            .child_max_tool_output_bytes
            .clamp(4 * 1024, 1024 * 1024);
        config.cluster.child_max_context_items =
            config.cluster.child_max_context_items.clamp(4, 200);
        config.cluster.child_max_context_bytes = config
            .cluster
            .child_max_context_bytes
            .clamp(16 * 1024, 1024 * 1024);
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

    /// Returns a saved connection profile, falling back to the active profile
    /// for compatibility with callers during the old-config migration.
    pub fn provider_for(&self, preset: ProviderPreset) -> Option<ProviderConfig> {
        self.providers
            .iter()
            .find(|provider| provider.preset == preset)
            .cloned()
            .or_else(|| (self.provider.preset == preset).then(|| self.provider.clone()))
    }

    /// Inserts or replaces a profile by preset. Keeping this centralized also
    /// prevents duplicate template additions from reaching the config file.
    pub fn upsert_provider(&mut self, provider: ProviderConfig) {
        if let Some(existing) = self
            .providers
            .iter_mut()
            .find(|existing| existing.preset == provider.preset)
        {
            *existing = provider;
        } else {
            self.providers.push(provider);
        }
    }

    pub fn remove_provider(&mut self, preset: ProviderPreset) -> Option<ProviderConfig> {
        let index = self
            .providers
            .iter()
            .position(|provider| provider.preset == preset)?;
        Some(self.providers.remove(index))
    }

    fn ensure_provider_profiles(&mut self) {
        let mut unique = Vec::with_capacity(self.providers.len() + 1);
        for provider in std::mem::take(&mut self.providers) {
            if let Some(existing) = unique
                .iter_mut()
                .find(|existing: &&mut ProviderConfig| existing.preset == provider.preset)
            {
                *existing = provider;
            } else {
                unique.push(provider);
            }
        }
        self.providers = unique;
        if !self.provider_profiles_initialized {
            self.upsert_provider(self.provider.clone());
            self.provider_profiles_initialized = true;
        }
    }
}

impl ProviderConfig {
    pub fn normalize_thinking(&mut self) {
        let profile = thinking_profile(self.preset, &self.model);
        if !profile.options.contains(&self.thinking_level) {
            self.thinking_level = profile.default;
        }
        if profile.kind != ThinkingProfileKind::Qwen37
            || self.thinking_level != ThinkingLevel::Enabled
            || !matches!(
                self.thinking_budget_tokens,
                None | Some(1024 | 4096 | 8192 | 16384 | 32768)
            )
        {
            self.thinking_budget_tokens = None;
        }
    }
    pub fn resolved_context_window_tokens(&self) -> Option<u64> {
        self.context_window_tokens
            .or_else(|| Some(known_context_window(self.preset, &self.model)))
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
        if !self.preset.supports_previous_response_id() {
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
                ProviderKind::Responses,
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
        let mut config = ProviderConfig {
            preset: self,
            kind,
            base_url: base_url.into(),
            model: model.into(),
            use_previous_response_id: false,
            native_web_search: NativeWebSearch::Auto,
            context_window_tokens: None,
            thinking: ThinkingCapability::Auto,
            thinking_level: ThinkingLevel::Auto,
            thinking_budget_tokens: None,
        };
        config.normalize_thinking();
        config
    }

    pub fn supports_responses(self) -> bool {
        matches!(
            self,
            Self::OpenAi | Self::DeepSeek | Self::Qwen | Self::Custom
        )
    }

    pub fn supports_previous_response_id(self) -> bool {
        !matches!(self, Self::DeepSeek)
    }

    /// Candidate models offered by the settings picker. `Custom` returns an
    /// empty list, so its model must be typed manually. Kept separate from the
    /// context-window lookup tables: those serve token estimation, not choice.
    pub fn selectable_models(self) -> &'static [&'static str] {
        match self {
            Self::OpenAi => OPENAI_SELECTABLE_MODELS,
            Self::DeepSeek => DEEPSEEK_SELECTABLE_MODELS,
            Self::Qwen => QWEN_SELECTABLE_MODELS,
            Self::Volcano => VOLCANO_SELECTABLE_MODELS,
            Self::Custom => &[],
        }
    }

    /// Inverse of `key_id`, used to recover a preset from its stored form.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|preset| preset.key_id() == value)
    }
}

const OPENAI_SELECTABLE_MODELS: &[&str] = &[
    "gpt-5-mini",
    "gpt-5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "o1",
    "o3",
    "o4-mini",
    "gpt-4o",
    "gpt-4.1",
    "o1-mini",
    "o1-preview",
];

const DEEPSEEK_SELECTABLE_MODELS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "deepseek-chat",
    "deepseek-reasoner",
];

const QWEN_SELECTABLE_MODELS: &[&str] = &[
    "qwen3.8-max",
    "qwen3.7-max",
    "qwen-plus",
    "qwen-max",
    "qwen-turbo",
    "qwen-long",
];

const VOLCANO_SELECTABLE_MODELS: &[&str] =
    &["doubao-seed-2-1-pro-260628", "deepseek-v4-flash", "glm-5.2"];

const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 258_000;

#[derive(Clone, Copy)]
struct ModelRule {
    model: &'static str,
    context_window_tokens: u64,
}

const OPENAI_EXACT_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "o1-mini",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "o1-preview",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "o1",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "o3",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "o4-mini",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "gpt-5.6-sol",
        context_window_tokens: 1_050_000,
    },
    ModelRule {
        model: "gpt-5.6-terra",
        context_window_tokens: 1_050_000,
    },
    ModelRule {
        model: "gpt-5.6-luna",
        context_window_tokens: 1_050_000,
    },
];

const OPENAI_PREFIX_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "o1-mini",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "o1-preview",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "o1",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "o3",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "o4-mini",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "gpt-5.6-sol",
        context_window_tokens: 1_050_000,
    },
    ModelRule {
        model: "gpt-5.6-terra",
        context_window_tokens: 1_050_000,
    },
    ModelRule {
        model: "gpt-5.6-luna",
        context_window_tokens: 1_050_000,
    },
    ModelRule {
        model: "gpt-4.1",
        context_window_tokens: 1_047_576,
    },
    ModelRule {
        model: "gpt-4o",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "gpt-5",
        context_window_tokens: 400_000,
    },
];

const DEEPSEEK_EXACT_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "deepseek-chat",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "deepseek-reasoner",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "deepseek-v4-pro",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "deepseek-v4-flash",
        context_window_tokens: 1_000_000,
    },
];

const DEEPSEEK_PREFIX_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "deepseek-r1",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "deepseek-v3",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "deepseek-v4-pro",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "deepseek-v4-flash",
        context_window_tokens: 1_000_000,
    },
];

const QWEN_EXACT_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "qwen-max",
        context_window_tokens: 32_768,
    },
    ModelRule {
        model: "qwen-plus",
        context_window_tokens: 131_072,
    },
    ModelRule {
        model: "qwen-turbo",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen-long",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen3.8-max",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen3.7-max",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen3.7-plus",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen3.7-flash",
        context_window_tokens: 1_000_000,
    },
];

const QWEN_PREFIX_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "qwen-max",
        context_window_tokens: 32_768,
    },
    ModelRule {
        model: "qwen-plus",
        context_window_tokens: 131_072,
    },
    ModelRule {
        model: "qwen-turbo",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen-long",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen3.8-max",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen3.7-max",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen3.7-plus",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen3.7-flash",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "qwen2.5",
        context_window_tokens: 131_072,
    },
    ModelRule {
        model: "qwen3",
        context_window_tokens: 131_072,
    },
];

const VOLCANO_PREFIX_MODELS: &[ModelRule] = &[
    ModelRule {
        model: "doubao-seed",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "deepseek-v4-flash",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "glm-5.2",
        context_window_tokens: 1_000_000,
    },
    ModelRule {
        model: "deepseek-v4-pro",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "glm-4.7",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "minimax-m2.7",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "minimax-m2.5",
        context_window_tokens: 200_000,
    },
    ModelRule {
        model: "doubao-seed-2.0-pro",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "doubao-seed-2.0-code",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "doubao-seed-2.0-lite",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "kimi-k2.6",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "kimi-k2.5",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "doubao-1-5-pro-32k",
        context_window_tokens: 32_000,
    },
    ModelRule {
        model: "doubao-1-5-lite-32k",
        context_window_tokens: 32_000,
    },
    ModelRule {
        model: "doubao-1-5-pro-128k",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "doubao-1-5-lite-128k",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "doubao-1-5-pro-256k",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "doubao-1-5-lite-256k",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "doubao-1-6-pro-32k",
        context_window_tokens: 32_000,
    },
    ModelRule {
        model: "doubao-1-6-lite-32k",
        context_window_tokens: 32_000,
    },
    ModelRule {
        model: "doubao-1-6-pro-128k",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "doubao-1-6-lite-128k",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "doubao-1-6-pro-256k",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "doubao-1-6-lite-256k",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "doubao-pro-32k",
        context_window_tokens: 32_000,
    },
    ModelRule {
        model: "doubao-lite-32k",
        context_window_tokens: 32_000,
    },
    ModelRule {
        model: "doubao-pro-128k",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "doubao-lite-128k",
        context_window_tokens: 128_000,
    },
    ModelRule {
        model: "doubao-pro-256k",
        context_window_tokens: 256_000,
    },
    ModelRule {
        model: "doubao-lite-256k",
        context_window_tokens: 256_000,
    },
];

fn known_context_window(preset: ProviderPreset, model: &str) -> u64 {
    let model = model.trim().to_ascii_lowercase();
    let matched = match preset {
        ProviderPreset::OpenAi => {
            lookup_model_window(&model, OPENAI_EXACT_MODELS, OPENAI_PREFIX_MODELS)
        }
        ProviderPreset::DeepSeek => {
            lookup_model_window(&model, DEEPSEEK_EXACT_MODELS, DEEPSEEK_PREFIX_MODELS)
        }
        ProviderPreset::Qwen => lookup_model_window(&model, QWEN_EXACT_MODELS, QWEN_PREFIX_MODELS),
        ProviderPreset::Volcano => lookup_model_window(&model, &[], VOLCANO_PREFIX_MODELS),
        ProviderPreset::Custom => {
            lookup_model_window(&model, OPENAI_EXACT_MODELS, OPENAI_PREFIX_MODELS)
                .or_else(|| {
                    lookup_model_window(&model, DEEPSEEK_EXACT_MODELS, DEEPSEEK_PREFIX_MODELS)
                })
                .or_else(|| lookup_model_window(&model, QWEN_EXACT_MODELS, QWEN_PREFIX_MODELS))
                .or_else(|| lookup_model_window(&model, &[], VOLCANO_PREFIX_MODELS))
        }
    };
    matched.unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS)
}

fn lookup_model_window(model: &str, exact: &[ModelRule], prefixes: &[ModelRule]) -> Option<u64> {
    exact
        .iter()
        .find(|rule| model == rule.model)
        .or_else(|| {
            prefixes
                .iter()
                .filter(|rule| model_family_matches(model, rule.model))
                .max_by_key(|rule| rule.model.len())
        })
        .map(|rule| rule.context_window_tokens)
}

fn model_family_matches(model: &str, family: &str) -> bool {
    model == family
        || model
            .strip_prefix(family)
            .is_some_and(|suffix| suffix.starts_with(['-', '.', ':']))
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
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn selectable_models_cover_defaults_and_custom_is_empty() {
        for preset in [
            ProviderPreset::OpenAi,
            ProviderPreset::DeepSeek,
            ProviderPreset::Qwen,
            ProviderPreset::Volcano,
        ] {
            let default_model = preset.defaults().model;
            assert!(preset.selectable_models().contains(&default_model.as_str()));
        }
        assert!(ProviderPreset::Custom.selectable_models().is_empty());
    }

    #[test]
    fn defaults_are_bounded() {
        let config = Config::default();
        assert_eq!(config.provider.kind, ProviderKind::Responses);
        assert!(config.runtime.max_fetch_bytes >= config.runtime.max_tool_output_bytes);
        assert_eq!(config.runtime.max_background_sessions, 8);
        assert_eq!(config.cluster.max_parallel_children, Some(4));
        assert_eq!(config.cluster.child_active_timeout_seconds, 300);
        assert!(config.compaction.enabled);
        assert_eq!(config.compaction.auto_threshold, 0.80);
    }

    #[test]
    fn background_session_limit_is_normalized() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runtime]\nmax_background_sessions = 0\n").unwrap();
        let low = Config::load(Some(&path), temp.path()).unwrap();
        assert_eq!(low.runtime.max_background_sessions, 2);

        fs::write(&path, "[runtime]\nmax_background_sessions = 1000\n").unwrap();
        let high = Config::load(Some(&path), temp.path()).unwrap();
        assert_eq!(high.runtime.max_background_sessions, 64);
    }

    #[test]
    fn compaction_limits_are_normalized() {
        let mut config = CompactionConfig {
            enabled: true,
            auto_threshold: 1.0,
            target_ratio: 0.1,
            preserve_recent_tokens: Some(100_000),
            max_summary_bytes: 1,
        };
        config.normalize();
        assert_eq!(config.auto_threshold, 0.90);
        assert_eq!(config.target_ratio, 0.30);
        assert_eq!(config.preserve_recent_tokens, Some(16_000));
        assert_eq!(config.max_summary_bytes, 4 * 1024);
    }

    #[test]
    fn legacy_active_provider_is_migrated_without_losing_fields() {
        let mut config = Config {
            provider: ProviderPreset::DeepSeek.defaults(),
            ..Config::default()
        };
        config.provider.base_url = "https://gateway.example/deepseek".into();
        config.provider.model = "deepseek-private".into();
        config.ensure_provider_profiles();

        let saved = config.provider_for(ProviderPreset::DeepSeek).unwrap();
        assert_eq!(saved.base_url, "https://gateway.example/deepseek");
        assert_eq!(saved.model, "deepseek-private");
    }

    #[test]
    fn provider_profiles_are_unique_and_serialized_without_secrets() {
        let mut config = Config::default();
        config.upsert_provider(ProviderPreset::Qwen.defaults());
        let mut replacement = ProviderPreset::Qwen.defaults();
        replacement.model = "qwen-custom-deployment".into();
        config.upsert_provider(replacement);

        assert_eq!(
            config
                .providers
                .iter()
                .filter(|provider| provider.preset == ProviderPreset::Qwen)
                .count(),
            1
        );
        assert_eq!(
            config.provider_for(ProviderPreset::Qwen).unwrap().model,
            "qwen-custom-deployment"
        );
        let encoded = toml::to_string(&config).unwrap();
        assert!(encoded.contains("providers"));
        assert!(!encoded.to_ascii_lowercase().contains("api_key"));
    }

    #[test]
    fn initialized_empty_profile_list_is_not_repopulated() {
        let mut config = Config {
            provider_profiles_initialized: true,
            ..Config::default()
        };
        config.ensure_provider_profiles();
        assert!(config.providers.is_empty());
    }

    #[test]
    fn deprecated_main_agent_turn_limit_is_ignored() {
        // RuntimeConfig intentionally accepts and ignores this removed key so
        // existing user configuration continues to load.
        let runtime: RuntimeConfig = toml::from_str(
            "max_agent_turns = 8\ncommand_timeout_seconds = 17\nmax_tool_output_bytes = 2048\nmax_fetch_bytes = 4096",
        )
        .unwrap();
        assert_eq!(runtime.command_timeout_seconds, 17);
        assert_eq!(runtime.max_tool_output_bytes, 2048);
        assert_eq!(runtime.max_fetch_bytes, 4096);
    }

    #[test]
    fn presets_have_expected_protocols_and_current_models() {
        let deepseek = ProviderPreset::DeepSeek.defaults();
        assert_eq!(deepseek.model, "deepseek-v4-flash");
        assert_eq!(deepseek.base_url, "https://api.deepseek.com");
        assert_eq!(deepseek.kind, ProviderKind::Responses);
        let qwen = ProviderPreset::Qwen.defaults();
        assert!(qwen.base_url.contains("{WorkspaceId}"));
        assert_eq!(qwen.kind, ProviderKind::ChatCompletions);
        assert!(ProviderPreset::Qwen.supports_responses());
        let volcano = ProviderPreset::Volcano.defaults();
        assert!(volcano.base_url.ends_with("/api/v3"));
    }

    #[test]
    fn thinking_profiles_match_provider_models_and_defaults() {
        let cases = [
            (
                ProviderPreset::OpenAi,
                "GPT-5.6-SOL-2026-08",
                ThinkingProfileKind::OpenAi,
                ThinkingLevel::Auto,
            ),
            (
                ProviderPreset::Qwen,
                "deployment-QWEN_3.8-MAX-v2",
                ThinkingProfileKind::Qwen38,
                ThinkingLevel::XHigh,
            ),
            (
                ProviderPreset::Qwen,
                "qwen-3.7-plus-latest",
                ThinkingProfileKind::Qwen37,
                ThinkingLevel::Enabled,
            ),
            (
                ProviderPreset::DeepSeek,
                "DEEPSEEK-V4-PRO-202608",
                ThinkingProfileKind::DeepSeekPro,
                ThinkingLevel::High,
            ),
            (
                ProviderPreset::DeepSeek,
                "tenant-deepseek_v4_flash",
                ThinkingProfileKind::DeepSeekFlash,
                ThinkingLevel::High,
            ),
            (
                ProviderPreset::Volcano,
                "deployment-id",
                ThinkingProfileKind::Volcano,
                ThinkingLevel::High,
            ),
            (
                ProviderPreset::Custom,
                "unknown-model",
                ThinkingProfileKind::Compatible,
                ThinkingLevel::Auto,
            ),
        ];
        for (preset, model, kind, default) in cases {
            let profile = thinking_profile(preset, model);
            assert_eq!(profile.kind, kind, "{model}");
            assert_eq!(profile.default, default, "{model}");
        }
        assert!(
            thinking_profile(ProviderPreset::DeepSeek, "deepseek-v4-flash")
                .options
                .contains(&ThinkingLevel::Max)
        );
        assert_eq!(
            thinking_profile(ProviderPreset::Volcano, "any").options,
            &[ThinkingLevel::High]
        );
    }

    #[test]
    fn thinking_config_serializes_and_old_config_uses_model_default() {
        let mut provider = ProviderPreset::DeepSeek.defaults();
        provider.thinking_level = ThinkingLevel::Max;
        let encoded = toml::to_string(&provider).unwrap();
        assert!(encoded.contains("thinking_level = \"max\""));
        let decoded: ProviderConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.thinking_level, ThinkingLevel::Max);

        let mut old: ProviderConfig = toml::from_str(
            r#"
preset = "qwen"
kind = "chat_completions"
base_url = "https://example.com/v1"
model = "qwen3.7-plus"
"#,
        )
        .unwrap();
        old.normalize_thinking();
        assert_eq!(old.thinking_level, ThinkingLevel::Enabled);
        assert_eq!(old.thinking_budget_tokens, None);
    }

    #[test]
    fn qwen_requires_workspace_id_before_saving() {
        let mut qwen = ProviderPreset::Qwen.defaults();
        assert!(qwen.validate().is_err());
        qwen.base_url = qwen.base_url.replace("{WorkspaceId}", "ws-example");
        assert!(qwen.validate().is_ok());
    }

    #[test]
    fn context_window_uses_provider_aware_registry_and_default() {
        let mut provider = ProviderPreset::DeepSeek.defaults();
        provider.model = "  DEEPSEEK-V3-0324  ".into();
        assert_eq!(provider.resolved_context_window_tokens(), Some(128_000));

        provider.model = "deepseek-v4-flash".into();
        assert_eq!(provider.resolved_context_window_tokens(), Some(1_000_000));
    }

    #[test]
    fn context_window_registry_covers_each_provider() {
        let cases = [
            (ProviderPreset::OpenAi, "gpt-5-mini", 400_000),
            (ProviderPreset::OpenAi, "gpt-5.6-sol", 1_050_000),
            (ProviderPreset::OpenAi, "gpt-5.6-terra", 1_050_000),
            (ProviderPreset::OpenAi, "gpt-5.6-luna", 1_050_000),
            (ProviderPreset::OpenAi, "gpt-4.1-mini", 1_047_576),
            (ProviderPreset::OpenAi, "gpt-4o-mini", 128_000),
            (ProviderPreset::OpenAi, "o1-mini", 128_000),
            (ProviderPreset::OpenAi, "o1-mini-2024-09-12", 128_000),
            (ProviderPreset::OpenAi, "o1", 200_000),
            (ProviderPreset::OpenAi, "o3", 200_000),
            (ProviderPreset::OpenAi, "o3-2025-04-16", 200_000),
            (ProviderPreset::OpenAi, "o4-mini", 200_000),
            (ProviderPreset::DeepSeek, "deepseek-chat", 128_000),
            (ProviderPreset::DeepSeek, "deepseek-reasoner", 128_000),
            (ProviderPreset::DeepSeek, "deepseek-r1-0528", 128_000),
            (ProviderPreset::DeepSeek, "deepseek-v4-pro", 1_000_000),
            (ProviderPreset::DeepSeek, "deepseek-v4-flash", 1_000_000),
            (ProviderPreset::Qwen, "qwen-max", 32_768),
            (ProviderPreset::Qwen, "qwen-plus", 131_072),
            (ProviderPreset::Qwen, "qwen-plus-latest", 131_072),
            (ProviderPreset::Qwen, "qwen-turbo", 1_000_000),
            (ProviderPreset::Qwen, "qwen-turbo-2025-xx", 1_000_000),
            (ProviderPreset::Qwen, "qwen-long", 1_000_000),
            (ProviderPreset::Qwen, "qwen3-235b-a22b", 131_072),
            (ProviderPreset::Qwen, "qwen3.8-max", 1_000_000),
            (ProviderPreset::Qwen, "qwen3.7-max", 1_000_000),
            (ProviderPreset::Qwen, "qwen3.7-plus", 1_000_000),
            (ProviderPreset::Qwen, "qwen3.7-flash", 1_000_000),
            (
                ProviderPreset::Volcano,
                "doubao-seed-2-1-pro-260628",
                256_000,
            ),
            (ProviderPreset::Volcano, "doubao-pro-32k-250115", 32_000),
            (ProviderPreset::Volcano, "deepseek-v4-flash", 1_000_000),
            (ProviderPreset::Volcano, "glm-5.2", 1_000_000),
            (ProviderPreset::Volcano, "deepseek-v4-pro", 200_000),
            (ProviderPreset::Volcano, "glm-4.7", 200_000),
            (ProviderPreset::Volcano, "minimax-m2.7", 200_000),
            (ProviderPreset::Volcano, "minimax-m2.5", 200_000),
            (ProviderPreset::Volcano, "doubao-seed-2.0-pro", 256_000),
            (ProviderPreset::Volcano, "doubao-seed-2.0-code", 256_000),
            (ProviderPreset::Volcano, "doubao-seed-2.0-lite", 256_000),
            (ProviderPreset::Volcano, "kimi-k2.6", 256_000),
            (ProviderPreset::Volcano, "kimi-k2.5", 256_000),
            (ProviderPreset::Volcano, "other-model-256k", 258_000),
            (ProviderPreset::Custom, "gpt-5-mini", 400_000),
            (ProviderPreset::Custom, "deepseek-chat", 128_000),
            (ProviderPreset::Custom, "qwen3-32b", 131_072),
            (
                ProviderPreset::Custom,
                "doubao-seed-2-1-pro-260628",
                256_000,
            ),
            (ProviderPreset::Custom, "deepseek-v4-flash", 1_000_000),
            (ProviderPreset::Custom, "vendor-model-128k", 258_000),
        ];
        for (preset, model, expected) in cases {
            let mut provider = preset.defaults();
            provider.model = model.into();
            assert_eq!(provider.resolved_context_window_tokens(), Some(expected));
        }
    }

    #[test]
    fn exact_model_rules_win_and_prefixes_use_longest_match() {
        assert_eq!(
            known_context_window(ProviderPreset::OpenAi, "O1-MINI"),
            128_000
        );
        assert_eq!(
            known_context_window(ProviderPreset::OpenAi, "gpt-4.1-mini"),
            1_047_576
        );
        assert_eq!(
            known_context_window(ProviderPreset::DeepSeek, "deepseek-r1"),
            128_000
        );
        assert_eq!(known_context_window(ProviderPreset::Qwen, "qwen3"), 131_072);
        assert_eq!(
            known_context_window(ProviderPreset::OpenAi, "o3foobar"),
            258_000
        );
        assert_eq!(
            known_context_window(ProviderPreset::Qwen, "qwen-plusfake"),
            258_000
        );
        assert_eq!(
            known_context_window(ProviderPreset::OpenAi, "gpt-5fake"),
            258_000
        );
    }

    #[test]
    fn custom_only_uses_explicit_known_vendor_families() {
        assert_eq!(
            known_context_window(ProviderPreset::Custom, "gpt-5"),
            400_000
        );
        assert_eq!(
            known_context_window(ProviderPreset::Custom, "unknown-32k"),
            258_000
        );
    }

    #[test]
    fn explicit_context_window_override_wins() {
        let mut provider = ProviderPreset::OpenAi.defaults();
        provider.context_window_tokens = Some(32_768);
        assert_eq!(provider.resolved_context_window_tokens(), Some(32_768));
    }

    #[test]
    fn deepseek_responses_is_stateless_and_native_search_defaults_to_auto() {
        let mut provider = ProviderPreset::DeepSeek.defaults();
        provider.use_previous_response_id = true;
        provider.validate().unwrap();
        assert!(!provider.use_previous_response_id);
        assert_eq!(provider.native_web_search, NativeWebSearch::Auto);
    }
}
