# 1H-Agent AI 维护协议

> 先读本文件，再按任务路由只读一个相关专题及目标源码；跨领域任务才组合读取，禁止为背景扫描整个仓库。

## 稳定上下文

```text
project: 1H-Agent（1H = 氕/protium）
goal: 极致轻量、高性能、权限感知的跨平台终端 Agent
runtime: 单个 Rust/Tokio 进程；Ratatui + Crossterm；SQLite/WAL
authority: 源码 > config/config.example.toml > .github/workflows > 本文件 > 专题指南
scope: TUI、模型流、受控工具、多会话、AI 集群、跨平台发布
excluded: Web UI、内置浏览器、远程 MCP、动态插件、图片和语音能力
```

保持单 Rust 二进制；不引入 Node.js、Python、Chromium、Web UI、动态插件 ABI 或后台轮询。所有路径、网络、工具、进程、缓存、channel 和输出必须有边界、取消与释放路径。

## 一分钟工作流

1. 先运行 `git status --short --branch`，识别并保护用户已有改动。
2. 用 `rg` 定位定义、直接调用者、事件变体和相邻测试；只读任务命中的专题。
3. 从 `src/main.rs -> app::run` 进入：全局状态在 `App`，单会话在 `SessionRuntime`，模型/工具循环在 `AgentRunner`。
4. 修改事件、配置或持久化类型时，覆盖所有构造点、match、序列化、恢复和测试。
5. 先跑最小目标测试；跨模块行为才升级到完整 Clippy 和测试。

## 任务路由

| 领域 | 首读入口 | 专题/读取条件 |
| --- | --- | --- |
| 启动、全局状态、会话路由 | `src/main.rs`、`src/app.rs`、`src/session.rs` | 无；仅沿目标事件链读取 |
| Provider、模型、密钥、协议、压缩恢复 | `src/config.rs`、`src/agent.rs`、`src/provider/openai.rs` | [Provider](.agents/guides/provider.md) |
| 子 Agent、审批、取消、集群停滞 | `src/agent.rs`、`src/app.rs` | [Cluster](.agents/guides/cluster.md) |
| 渲染、长文本、滚动、鼠标交互 | `src/app.rs`、`src/ui.rs`、`src/output.rs` | [TUI](.agents/guides/tui.md) |
| 工具、路径、SSRF、外部进程 | `src/tools/`、`src/security.rs` | 无；遵守全局安全规则 |
| 会话、分支、迁移、持久化 | `src/storage.rs`、`src/session.rs` | 涉及 Provider 状态时再读 Provider |
| CI、版本、安装包、tag | `.github/workflows/`、`Cargo.toml` | [Release](.agents/guides/release.md) |

指南与源码不一致时以源码为准，并在同一改动中更新该指南；一个事实只归属根文档或一个专题。

## 架构与全局不变量

```text
terminal event -> App -> SessionRuntime -> AgentRunner -> OpenAiClient / ToolRegistry
                     |          |               |
                     +-> UI     +-> Storage     +-> RoutedEvent(session_id) -> App
```

- `App` 管全局 UI、当前/后台 runtime 和路由；`SessionRuntime` 独占单会话状态，切换不停止后台任务。
- Provider 私有协议先规范化为 `ModelEvent`；UI、存储和工具层不解析私有 JSON。
- 恢复沿 `head_turn_id` 父链；fork 不复制 Provider 服务端状态；undo/redo 只移动 head。
- workspace 必须 canonicalize；拒绝绝对路径、`..`、符号链接逃逸；新目标验证 canonical parent。
- Web 每次重定向都校验 HTTP/HTTPS 和公网地址；危险操作始终经过 mode、安全分类与审批。
- API Key 只来自环境变量或系统钥匙串，不进入 TOML、SQLite、日志、导出或模型上下文。
- 外部进程必须支持超时、输出截断、取消和进程树清理；`Esc` 产生可观察终态。
- 新增容量或并发前定义硬上限、截断、取消与释放；未知模型使用显式窗口或 Provider 感知注册表。

## 实施与验证

| 改动 | 最小验证 |
| --- | --- |
| 文档 | `bash scripts/check-agent-docs.sh`、`git diff --check` |
| 局部 Rust | `cargo fmt --all -- --check` 加目标测试过滤器 |
| 跨模块/公共行为 | `cargo clippy --all-targets --all-features --locked -- -D warnings`、`cargo test --all-features --locked` |
| 发布 | 读取 Release 专题并运行其完整验证 |

保持改动聚焦，复用现有 helper，不清理无法证明无用的文件。未运行的检查必须在最终回复说明；不要因 Cargo 锁或冷缓存终止正常构建。
