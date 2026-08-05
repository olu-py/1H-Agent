# 1H-Agent Technical Design

## Architecture

1H-Agent is one Tokio process. Crossterm produces terminal events, Ratatui
renders immutable snapshots, and bounded channels carry model and tool events
back to the application state. There is no periodic redraw while idle.

```text
Terminal -> App state -> Agent runner -> OpenAI provider
                |              |
                |              +-> policy -> tool registry
                +-> SQLite <------------ audit/result events
```

The model layer uses normalized `ConversationItem`, `ModelRequest`,
`ModelEvent`, `ToolCall`, `ToolDefinition`, and `Usage` types. Chat Completions
and Responses wire formats are isolated in their respective serializers and
SSE parsers. The Agent runner owns the bounded multi-turn loop: collect a
response, approve/execute requested tools, append function outputs, then call
the model again up to the configured limit.

## OpenAI protocols

The official default base URL is `https://api.openai.com/v1`; an operator may
select another base URL. Both adapters use bearer authentication, JSON request
bodies, SSE streaming, explicit timeouts, and status/body error capture. Chat
serializes conventional message/tool-call/tool-result messages. Responses
serializes message, function-call, and function-call-output items and records a
returned response identifier for optional server-side continuation.

API keys are read from `OPENAI_API_KEY` first and then the OS keyring service
(`1h-agent/openai`). Keys are never serialized, persisted, or logged. The
provider schema must be checked against the official OpenAI OpenAPI contract
before a release; compatibility endpoints are allowed to implement only the
features they advertise.

## Tools and policy

All tool input is JSON validated by typed deserialization. The registry exposes
JSON Schema function definitions to the model and dispatches only known names.

- File tools list, stat, read, search, mkdir, write, copy, move, and delete.
- Web fetch supports HTTP/HTTPS GET or HEAD, five redirects, a ten MiB default
  limit, content-type reporting, and HTML-to-text conversion.
- Process execution receives a program, argument vector, canonical working
  directory, timeout, and output limit. It does not interpolate a shell unless
  the explicit shell mode is approved.
- Git is the installed executable with a validated subcommand/argument vector;
  repository reads may auto-run, while mutations and network operations do not.

The workspace root is canonicalized once. Existing targets are canonicalized
before access. New targets require a canonical in-workspace parent. This blocks
absolute-path, `..`, and symlink escapes. Web targets are resolved and each IP
is checked; loopback, private, link-local, and unspecified addresses are denied
by default, including redirects.

Policy outcomes are `Allow`, `RequireApproval(reason)`, or `Deny(reason)`.
Writes, deletes, process execution, shell mode, Git mutations/remotes, and any
requested workspace escape require approval or are denied. Every decision and
execution result is recorded without secret values.

## Persistence and configuration

SQLite uses bundled SQLite and WAL mode. Migrations create sessions, messages,
tool calls, and provider state. Large tool output is truncated in memory; a
future attachment store may retain an explicitly requested full output.

Configuration precedence is CLI, environment, user TOML, defaults. Non-secret
configuration is stored below the OS configuration directory; the SQLite file
is below the OS data directory. If those directories are unavailable, the app
uses `.1h-agent` below the workspace. Recognized environment variables include
`OPENAI_API_KEY`, `AGENT_API_BASE`, `AGENT_MODEL`, `AGENT_PROVIDER`, and
`AGENT_DATA_DIR`.

## Cross-platform behavior

Linux processes start in their own process group and the group is terminated on
timeout. Windows uses a new process group and `taskkill /T` as the process-tree
fallback. Input, resize, UTF-8 rendering, Ctrl+C, and terminal restoration are
tested natively on both systems. Release CI builds portable archives first;
`cargo-deb` and WiX packaging are release gates rather than runtime dependencies.

## Verification

Unit fixtures cover fragmented SSE frames, Chat and Responses text/tool events,
path/symlink escapes, private-address rejection, approval classification,
redaction, output limits, and config precedence. Integration tests use temporary
workspaces and Git repositories. CI runs formatting checks, Clippy with warnings
denied, tests, and release builds on Linux and Windows. Linux additionally checks
the musl build when system prerequisites are available.
