use arboard::Clipboard;
use base64::{Engine as _, engine::general_purpose::STANDARD};

pub const MAX_CLIPBOARD_BYTES: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum CopyResult {
    Native,
    Osc52Requested(String),
    Error(String),
}

pub fn copy_text(text: &str) -> CopyResult {
    copy_text_with(text, |value| {
        let mut clipboard = Clipboard::new().map_err(|error| error.to_string())?;
        clipboard.set_text(value).map_err(|error| error.to_string())
    })
}

pub fn copy_text_with<F>(text: &str, set_native: F) -> CopyResult
where
    F: FnOnce(&str) -> Result<(), String>,
{
    if text.len() > MAX_CLIPBOARD_BYTES {
        return CopyResult::Error(format!(
            "选区超过 {} KiB 剪贴板上限",
            MAX_CLIPBOARD_BYTES / 1024
        ));
    }
    match set_native(text) {
        Ok(()) => CopyResult::Native,
        Err(_) => CopyResult::Osc52Requested(osc52_sequence(text)),
    }
}

pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_copy_success_does_not_emit_osc52() {
        assert_eq!(copy_text_with("hello", |_| Ok(())), CopyResult::Native);
    }

    #[test]
    fn native_failure_falls_back_to_base64_osc52() {
        let result = copy_text_with("中文\n\x1b", |_| Err("unavailable".into()));
        let CopyResult::Osc52Requested(sequence) = result else {
            panic!("expected OSC 52 fallback");
        };
        assert!(sequence.starts_with("\x1b]52;c;"));
        assert!(sequence.ends_with('\x07'));
        let payload = sequence
            .strip_prefix("\x1b]52;c;")
            .and_then(|value| value.strip_suffix('\x07'))
            .expect("standard OSC 52 framing");
        let decoded = STANDARD.decode(payload).expect("base64 OSC 52 payload");
        assert_eq!(decoded, "中文\n\x1b".as_bytes());
        assert!(!payload.contains("中文"));
        assert!(!payload.contains('\x1b'));
    }

    #[test]
    fn clipboard_limit_is_inclusive_and_does_not_call_native() {
        let mut called = false;
        let result = copy_text_with(&"x".repeat(MAX_CLIPBOARD_BYTES), |_| {
            called = true;
            Ok(())
        });
        assert_eq!(result, CopyResult::Native);
        assert!(called);

        let result = copy_text_with(&"x".repeat(MAX_CLIPBOARD_BYTES + 1), |_| {
            panic!("native clipboard must not be touched");
        });
        assert!(matches!(result, CopyResult::Error(_)));
    }
}
