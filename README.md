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
export OPENAI_API_KEY="..."
export AGENT_MODEL="gpt-5-mini"
cargo run -- --workspace /path/to/project
```

Configuration can be copied from `config/config.example.toml`. Environment
variables override the file. Press `Ctrl+C` to quit, `Esc` to cancel an active
request, and `Y` or `N` when a tool approval dialog is open.

`AGENT_DATA_DIR` overrides the platform data directory, which is useful in
portable or sandboxed environments.

## Documentation

- [Development plan](PLAN.md)
- [Technical design](TECHNICAL_DESIGN.md)
