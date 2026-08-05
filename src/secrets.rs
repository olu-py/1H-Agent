use std::env;

use thiserror::Error;

const SERVICE: &str = "1h-agent";
const USER: &str = "openai";

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("OPENAI_API_KEY is not set and no key was found in the system keyring")]
    Missing,
    #[error("system keyring error: {0}")]
    Keyring(String),
}

pub fn openai_api_key() -> Result<String, SecretError> {
    if let Ok(key) = env::var("OPENAI_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }

    let entry = keyring::Entry::new(SERVICE, USER)
        .map_err(|error| SecretError::Keyring(error.to_string()))?;
    match entry.get_password() {
        Ok(key) if !key.trim().is_empty() => Ok(key),
        Ok(_) | Err(keyring::Error::NoEntry) => Err(SecretError::Missing),
        Err(error) => Err(SecretError::Keyring(error.to_string())),
    }
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
            redact("Bearer sk-example123456789 end"),
            "Bearer [REDACTED] end"
        );
        assert_eq!(redact("ordinary text"), "ordinary text");
    }
}
