# 1H-Agent

1H-Agent is a lightweight, permission-aware terminal agent for Linux and
Windows. It uses a native Rust TUI and supports both OpenAI Chat Completions
and Responses endpoints.

## Status

This repository contains the first working implementation. The security model
is intentionally conservative: file access is constrained to the selected
workspace and commands, mutations, and remote Git operations require approval.

## Requirements

- Rust stable for building
- Git for Git tools
- A UTF-8 terminal
- `OPENAI_API_KEY` for OpenAI requests

## Run

```bash
cargo run -- --workspace /path/to/project
```

Open Provider Settings with `Ctrl+S`. Use the arrow keys to select OpenAI,
DeepSeek, Qwen/Bailian, Volcano Ark, or a custom OpenAI-compatible endpoint.
Use `Tab` to move between fields, edit the Base URL/model/API Key, and press
`Enter` to apply immediately. `Esc` closes the panel without saving.

The API Key is stored under a provider-specific account in the OS keyring. If
secure storage is unavailable, the key remains usable only for the current
process. Environment variables remain supported for unattended use:

| Preset | Environment variable | Default model |
| --- | --- | --- |
| OpenAI | `OPENAI_API_KEY` | `gpt-5-mini` |
| DeepSeek | `DEEPSEEK_API_KEY` | `deepseek-v4-flash` |
| Qwen/Bailian | `DASHSCOPE_API_KEY` | `qwen3.8-max` |
| Volcano Ark | `ARK_API_KEY` | `doubao-seed-2-1-pro-260628` |
| Custom | `AGENT_API_KEY` | editable |

Qwen's current China-compatible endpoint contains an account-specific
`WorkspaceId`. Replace the placeholder in Provider Settings before saving.

Configuration can be copied from `config/config.example.toml`. Environment
variables override the file. Press `Ctrl+C` to quit, `Esc` to cancel an active
request, and `Y` or `N` when a tool approval dialog is open.

`AGENT_DATA_DIR` overrides the platform data directory, which is useful in
portable or sandboxed environments.

## Documentation

- [Development plan](PLAN.md)
- [Technical design](TECHNICAL_DESIGN.md)
