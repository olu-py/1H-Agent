# 1H-Agent Development Plan

## Goal

Deliver a low-memory TUI agent for Linux and Windows that can converse through
OpenAI Chat Completions or Responses and safely operate on a selected local
workspace. The process stays native and single-binary; Node.js, Python, Docker,
and a local model are not runtime dependencies.

## Delivery phases

1. **Foundation**: Rust project, event-driven TUI, configuration, SQLite
   migrations, structured logging, clean terminal shutdown.
2. **Model loop**: common conversation model, Chat and Responses adapters, SSE
   streaming, function-call accumulation, usage/error normalization, cancel.
3. **Tools**: workspace files, public HTTP fetch, controlled processes, Git,
   approval flow, timeouts, bounded output, audit records.
4. **Hardening**: path and symlink escape tests, SSRF checks, secret redaction,
   process-tree cleanup, crash recovery, Windows behavior tests.
5. **Release**: Linux `tar.gz` and `deb`, Windows `zip` and `msi`, hashes,
   native CI builds, startup and memory benchmarks.

## Acceptance criteria

- The program starts without a Node.js or Python runtime.
- All filesystem operations remain below the canonical workspace root.
- Mutating, process, shell, and remote Git actions cannot run without approval.
- Chat and Responses streams render incrementally and can be cancelled.
- Tool output and channels are bounded; large output cannot grow memory without
  limit.
- Terminal modes are restored after normal exit, cancellation, or an error.
- Linux and Windows native CI compile and run the automated test suite.
- Normal interactive memory is targeted at 20-50 MB and idle CPU at effectively
  zero; measured results are recorded before the first release.

## Environment

The approved environment is `1H-Agent Rust development toolchain`: rustup
minimal, Rust stable, Cargo, rustfmt, Clippy, and Linux GNU/musl targets. The
toolchain and build cache have a 5 GB budget. Windows artifacts are built on a
native Windows CI runner instead of installing MinGW/WiX locally.

## Current release boundaries

The first release excludes a headless browser, desktop automation, local model,
voice, a plugin ABI, and provider-native Anthropic/Gemini protocols. `webfetch`
is HTTP only. OpenAI-compatible servers are supported through a custom base URL
on a conservative subset of the official protocols.

## Verification snapshot

On the Linux development host, formatting, Clippy with warnings denied, 15 unit
tests, 2 integration tests, and the optimized GNU release build pass. The
release binary is approximately 6.3 MiB. An idle TUI process measured 8.3 MiB
RSS and 0.0% CPU, and `Ctrl+C` restored the terminal normally. The musl check
currently requires the approved `musl-tools` system package; Windows compilation
and installers are defined in CI and must be confirmed on a native runner.
