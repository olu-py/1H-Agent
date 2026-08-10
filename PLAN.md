# 1H-Agent Development Plan

项目名中的 `1H` 指氕（protium，氢-1 同位素），代表轻量化的核心方向。

## Goal

Deliver a low-memory TUI agent for Linux, macOS, and Windows that can converse
through OpenAI Chat Completions or Responses and safely operate on a selected
local workspace. The process stays native and single-binary; Node.js, Python,
Docker, and a local model are not runtime dependencies.

## Delivery phases

1. **Foundation**: Rust project, event-driven TUI, configuration, SQLite
   migrations, structured logging, clean terminal shutdown.
2. **Model loop**: common conversation model, Chat and Responses adapters, SSE
   streaming, function-call accumulation, usage/error normalization, cancel.
3. **Tools**: workspace files, public HTTP fetch, controlled processes, Git,
   approval flow, timeouts, bounded output, audit records.
4. **Hardening**: path and symlink escape tests, SSRF checks, secret redaction,
   process-tree cleanup, crash recovery, Windows and macOS behavior tests.
5. **Release**: Linux `tar.gz` and `deb`, Windows `zip` and `msi`, macOS Intel
   and Apple Silicon `tar.gz`, hashes, native CI builds, startup and memory
   benchmarks.

## Acceptance criteria

- The program starts without a Node.js or Python runtime.
- All filesystem operations remain below the canonical workspace root.
- Mutating, process, shell, and remote Git actions cannot run without approval.
- Chat and Responses streams render incrementally and can be cancelled.
- Tool output and channels are bounded; large output cannot grow memory without
  limit.
- Terminal modes are restored after normal exit, cancellation, or an error.
- Linux, macOS, and Windows native CI compile and run the automated test suite.
- Normal interactive memory is targeted at 20-50 MB and idle CPU at effectively
  zero; measured results are recorded before the first release.

## Environment

The approved environment is `1H-Agent Rust development toolchain`: rustup
minimal, Rust stable, Cargo, rustfmt, Clippy, and Linux GNU/musl targets. The
toolchain and build cache have a 5 GB budget. Windows artifacts are built on a
native Windows CI runner instead of installing MinGW/WiX locally; macOS release
archives cover both Intel and Apple Silicon.

## Current release boundaries

The lightweight TUI includes OpenCode-style commands, multi-line input,
bounded file references, approved `!` shell commands, session branching,
undo/redo, compaction, export, diff viewing, Plan/Build/Explore modes, and a
bounded one-level child-agent request. Browser automation is external only:
the optional JSONL bridge is a user-installed process, and no Chromium is
bundled. A minimal local stdio MCP subset can discover and call tools when
explicitly enabled. Remote sharing, Web UI, voice, a plugin ABI/marketplace,
and provider-native Anthropic/Gemini protocols remain excluded.

## Verification snapshot

On the development host, formatting, Clippy with warnings denied, 45 unit
tests, 2 integration tests, and the optimized release build pass. The current
release binary is approximately 5.6 MiB. Existing idle measurements remain the
baseline: about 8.3 MiB RSS on Linux, 3.3 MiB on macOS, and effectively zero
idle CPU. The musl check requires the approved `musl-tools` system package;
Windows compilation/installers and both macOS architectures are defined in CI
and must be confirmed on their hosted runners.
