# 1H-Agent

`1H` 指氕（protium），即氢元素最轻、最常见的氢-1 同位素；项目名称表达
轻量、基础而高效的 Agent 核心，而不是泛指的“1 小时”。

1H-Agent is a lightweight, permission-aware terminal agent for Linux and
Windows. It uses a native Rust TUI and supports both OpenAI Chat Completions
and Responses endpoints.

## Status

This repository contains a working lightweight TUI implementation. The
security model is intentionally conservative: file access is constrained to
the selected workspace and commands, shell, mutations, browser interactions,
and remote Git operations require approval.

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
variables override the file. The TUI keeps the main controls visible in its
footer: `Enter` sends, `Ctrl+S` opens provider settings, `Alt+Up/Down` switches
sessions, `Ctrl+N` creates a session, `Esc` cancels an active request, and
`Ctrl+C` quits. OpenCode-style controls are also available: `Ctrl+X` is the
leader key, `Ctrl+P` opens the command palette, `/` runs slash commands, `@`
offers workspace file references, `!` runs an approved workspace shell command,
`PageUp/PageDown` scroll messages, and `Ctrl+O` folds tool details. Use `Y` or
`N` when a tool approval dialog is open.

The two-line activity display keeps operating mode, Agent phase, and model
stream state separate. Thinking summaries are bounded progress descriptions,
not raw chain-of-thought. Assistant text and tool calls retain their event
order (`text -> tool -> text`), including after a session is reloaded. Approval
dialogs translate common tool arguments into paths, commands, sizes, risk, and
other readable fields while redacting secret-like values.

The TUI uses Simplified Chinese labels and includes a bounded context-window
ring below the Agent/model state. The limit is resolved from an explicit
`provider.context_window_tokens` override or a small built-in registry of known
model limits. Unknown models display an unknown limit instead of assuming
128,000 tokens. Provider input-token usage takes precedence over the local
allocation-free estimate. At 85% it recommends `/compact`.

DeepSeek's public Chat Completions API does not expose the native search feature
from its consumer client. 1H-Agent therefore provides a lightweight
`web_search` function tool backed by a bounded, text-only public search request,
plus `web_fetch` for reading a selected URL. Both use the same redirect, output,
timeout, and SSRF controls and require no browser runtime.

Built-in slash commands include `/help`, `/new`, `/sessions`, `/rename`,
`/delete`, `/fork`, `/undo`, `/redo`, `/compact`, `/export`, `/diff`,
`/model`, `/provider`, `/agent`, `/plan`, `/build`, `/explore`, `/clear`, and
`/quit`. Custom prompt templates, per-tool permissions, external browser
bridges, and local stdio MCP tool servers are configured in TOML. These are
user-supplied processes; 1H-Agent does not bundle a browser runtime or plugin
ABI.

`AGENT_DATA_DIR` overrides the platform data directory, which is useful in
portable or sandboxed environments.

## Documentation

- [Development plan](PLAN.md)
- [Technical design](TECHNICAL_DESIGN.md)
