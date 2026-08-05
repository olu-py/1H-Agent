use std::env;

use thiserror::Error;

use crate::config::ProviderPreset;

const SERVICE: &str = "1h-agent";

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("no API key is configured for {0}")]
    Missing(String),
    #[error("system keyring error: {0}")]
    Keyring(String),
}

pub fn api_key(preset: ProviderPreset) -> Result<String, SecretError> {
    let variables: &[&str] = match preset {
        ProviderPreset::OpenAi => &["OPENAI_API_KEY", "AGENT_API_KEY"],
        ProviderPreset::DeepSeek => &["DEEPSEEK_API_KEY", "AGENT_API_KEY"],
        ProviderPreset::Qwen => &["DASHSCOPE_API_KEY", "QWEN_API_KEY", "AGENT_API_KEY"],
        ProviderPreset::Volcano => &["ARK_API_KEY", "VOLCANO_API_KEY", "AGENT_API_KEY"],
        ProviderPreset::Custom => &["AGENT_API_KEY"],
    };
    for variable in variables {
        if let Ok(key) = env::var(variable) {
            if !key.trim().is_empty() {
                return Ok(key);
            }
        }
    }

    let entry = keyring::Entry::new(SERVICE, preset.key_id())
        .map_err(|error| SecretError::Keyring(error.to_string()))?;
    match entry.get_password() {
        Ok(key) if !key.trim().is_empty() => Ok(key),
        Ok(_) | Err(keyring::Error::NoEntry) => Err(SecretError::Missing(preset.label().into())),
        Err(error) => Err(SecretError::Keyring(error.to_string())),
    }
}

pub fn store_api_key(preset: ProviderPreset, api_key: &str) -> Result<(), SecretError> {
    if api_key.trim().is_empty() {
        return Err(SecretError::Missing(preset.label().into()));
    }
    let entry = keyring::Entry::new(SERVICE, preset.key_id())
        .map_err(|error| SecretError::Keyring(error.to_string()))?;
    entry
        .set_password(api_key)
        .map_err(|error| SecretError::Keyring(error.to_string()))
}

pub fn redact(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| {
            if token.starts_with("sk-") && token.len() > 12 {
                "[REDACTED]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_key_like_tokens() {
        assert_eq!(
            redact(&format!("Bearer {}{} end", "sk-", "example123456789")),
            "Bearer [REDACTED] end"
        );
        assert_eq!(redact("ordinary text"), "ordinary text");
    }
}
