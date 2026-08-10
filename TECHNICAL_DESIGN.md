# 1H-Agent Technical Design

`1H` 指氕（protium，氢-1 同位素）。名称强调项目以最小运行时和最少状态
提供高性能 Agent 能力。

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

The TUI input path is dependency-free. `InputBuffer` stores a UTF-8 byte
cursor, bounded in-memory history, and multi-line editing state. `Command` is a
static command enum used by slash commands, the `Ctrl+P` palette, and the
`Ctrl+X` leader key. Crossterm events are the only source of redraws while
idle; visible message lines and folded tool outputs are rendered from bounded
snapshots.

Runtime presentation uses independent `AgentPhase` and `ModelPhase` state
machines. Model streaming therefore does not imply that the Agent is thinking,
waiting for approval, or running a tool. Display entries are appended in event
order, so a later text delta creates a new assistant segment after the latest
tool result instead of mutating an earlier assistant block. Bounded thinking
summaries and tool call/output items are persisted as typed message rows; they
are restored for display but thinking summaries are excluded from provider
request serialization.

The footer also renders an allocation-free context estimate as a fixed-width
Unicode ring. Provider-reported input tokens take precedence over the local
conversation-byte estimate. Capacity comes from an explicit per-provider
override or an exact/prefix model registry; unknown models show an unknown
limit. Explicit limits are bounded between 4,096 and 10,000,000 tokens.

DeepSeek Responses requests may include the provider-hosted
`{"type":"web_search"}` tool. When enabled, the local function with the same
name is omitted from that request. Search status events and URL citations are
bounded before display, while completed `reasoning` and `web_search_call` items
are persisted and replayed because DeepSeek Responses is stateless. Chat and
other providers retain the bounded text-search function and SSRF-protected
`web_fetch`; no browser or search runtime is bundled.

## OpenAI protocols

The official default base URL is `https://api.openai.com/v1`; an operator may
select another base URL. Both adapters use bearer authentication, JSON request
bodies, SSE streaming, explicit timeouts, and status/body error capture. Chat
serializes conventional message/tool-call/tool-result messages. Responses
serializes message, function-call, and function-call-output items and records a
returned response identifier for optional server-side continuation.

Provider Settings is an in-TUI configuration surface. Saving rebuilds the
provider and Agent runner immediately; no restart is required. Non-secret
provider settings go to TOML, while API keys use provider-specific OS keyring
accounts below the `1h-agent` service. If secure storage is unavailable, a newly
entered key remains in process memory for that session only. Keys are never
serialized to TOML, SQLite, or logs.

Built-in presets use current official compatibility settings: OpenAI;
DeepSeek Responses (`https://api.deepseek.com`, `deepseek-v4-flash`); Qwen/Bailian
(`https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1`,
`qwen3.8-max`); and Volcano Ark
(`https://ark.cn-beijing.volces.com/api/v3`,
`doubao-seed-2-1-pro-260628`). Qwen and Volcano default to Chat Completions;
unsupported Responses selection is forced back to Chat during validation.
DeepSeek never sends `previous_response_id`, because its Responses endpoint is
stateless and requires the relevant input items to be replayed locally.

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
- `git_diff` is a read-only fixed-argument diff view. Direct `!` commands use
  the platform shell only after an explicit approval and retain bounded output.
- `browser_*` tools and `mcp:<server>:<tool>` tools are process-backed JSONL
  adapters. They are started only when configured/enabled, have timeouts and
  output caps, and never load a browser runtime or dynamic plugin ABI.

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
future attachment store may retain an explicitly requested full output. The
current migration adds session mode, soft deletion, turn heads, parent/child
branches, message kinds, hidden rows, and bounded context attachments. Undo
and redo move the session head through the turn tree; a new prompt after undo
creates a new branch.
The session sidebar is backed by the workspace-filtered SQLite session list;
`Alt+Up/Down` reloads the selected history and rebinds the runner, while
`Ctrl+N` creates and activates a new session.

Configuration also supports per-tool `allow`/`ask`/`deny` overrides, custom
prompt commands, named Plan/Build/Explore agents, an external browser bridge,
and a minimal local stdio MCP server list. Configuration never stores API keys.

Configuration precedence is CLI, environment, user TOML, defaults. Non-secret
configuration is stored below the OS configuration directory; the SQLite file
is below the OS data directory. If those directories are unavailable, the app
uses `.1h-agent` below the workspace. Recognized environment variables include
`OPENAI_API_KEY`, `DEEPSEEK_API_KEY`, `DASHSCOPE_API_KEY`, `ARK_API_KEY`,
`AGENT_API_KEY`, `AGENT_API_BASE`, `AGENT_MODEL`, `AGENT_PROVIDER`, and
`AGENT_DATA_DIR`.

## Cross-platform behavior

Linux and macOS processes start in their own process group and the group is
terminated on timeout. Windows uses a new process group and `taskkill /T` as
the process-tree fallback. Input, resize, UTF-8 rendering, Ctrl+C, and terminal
restoration are tested natively on all three systems. Release CI builds
portable archives first; `cargo-deb` and WiX packaging are release gates rather
than runtime dependencies.

## Verification

Unit fixtures cover fragmented SSE frames, Chat and Responses text/tool events,
path/symlink escapes, private-address rejection, approval classification,
redaction, output limits, and config precedence. Integration tests use temporary
workspaces and Git repositories. CI runs formatting checks, Clippy with warnings
denied, tests, and release builds on Linux, macOS, and Windows. Linux additionally
checks the musl build when system prerequisites are available.
