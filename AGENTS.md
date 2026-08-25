# 1H-Agent AI 维护协议

> 先读本文件，再按任务路由只读一个相关专题及目标源码；跨领域任务才组合读取，禁止为背景扫描整个仓库。

## 稳定上下文

```text
project: 1H-Agent（1H = 氕/protium）
goal: 极致轻量、高性能、权限感知的跨平台 Agent；protium-core 独立演进，TUI/WebUI/Desktop 为消费端适配器
runtime: 核心 protium-core（Rust/Tokio，SQLite/WAL）；消费端只经通用接口连接核心
authority: 源码 > config/config.example.toml > .github/workflows > 本文件 > 专题指南
scope: TUI adapter、projection/渲染/输入、通用协议适配、跨平台发布
excluded: 本仓库不实现 WebUI/Desktop 源码（仅契约覆盖接入约束）、内置浏览器、远程 MCP、动态插件、图片和语音能力
```

后端核心为单 Rust 库/二进制，可独立发布和迭代；当前仓库保留单进程 TUI 运行方式，但 TUI 不是核心状态机。不引入 Node.js、Python、Chromium、WebUI/Desktop UI 源码、动态插件 ABI 或后台轮询。所有路径、网络、工具、进程、缓存、channel 和输出必须有边界、取消与释放路径。

## 一分钟工作流

1. 先运行 `git status --short --branch`，识别并保护用户已有改动。
2. 用 `rg` 定位定义、直接调用者、事件变体和相邻测试；只读任务命中的专题。
3. 从 `src/main.rs -> app::run` 进入：TUI 门面是 `src/app.rs`（`App` + `TuiSessionProjection`）；核心接口在独立 `1H-Agent-core` 仓库的 `src/service.rs`、`protocol.rs`、`bridge.rs`。
4. 修改事件、配置或持久化类型时，覆盖所有构造点、match、序列化、恢复和测试。
5. 先跑最小目标测试；跨模块行为才升级到完整 Clippy 和测试。
6. core 变更先在独立仓库完成并 push；本仓库只定向更新 Git 依赖、适配和提交锁文件，禁止编辑 Cargo checkout。

## 任务路由

| 领域 | 首读入口 | 专题/读取条件 |
| --- | --- | --- |
| 通用 UI 契约、事件游标/回放、resync、协议一致性夹具 | core 仓库 `src/protocol.rs`、`bridge.rs`、`service.rs`、`conformance.rs` | [UI Contract](.agents/guides/ui-contract.md) |
| 启动、全局状态、会话路由 | `protium-core (Git dependency): src/service.rs`、`protium-core (Git dependency): src/app.rs`；TUI 门面 `src/app.rs` | [Runtime](.agents/guides/runtime.md)；仅沿目标事件链读取 |
| Provider、模型、密钥、协议、压缩恢复 | `protium-core (Git dependency): src/config.rs`、`agent.rs`、`provider/openai.rs` | [Provider](.agents/guides/provider.md) |
| 子 Agent、审批、取消、集群停滞 | `protium-core (Git dependency): src/agent.rs`、`service.rs` | [Cluster](.agents/guides/cluster.md) |
| TUI projection、渲染、长文本、滚动、鼠标（门面非核心） | `src/projection.rs`、`src/app.rs`、`src/ui.rs`、`src/output.rs`、`src/ui_view_model.rs` | [TUI](.agents/guides/tui.md) |
| 工具、路径、SSRF、外部进程 | `protium-core (Git dependency): src/tools/`、`security.rs` | [Tools](.agents/guides/tools.md) |
| 会话、分支、迁移、持久化 | `protium-core (Git dependency): src/storage.rs`、`session.rs` | [Storage](.agents/guides/storage.md)；涉及 Provider 状态时再读 Provider |
| 配置上限、容量归一化、新增配置键 | `protium-core (Git dependency): src/config.rs` 的 `Config::load` clamp 区、`config/config.example.toml` | Provider 专题（容量预算、`max_output_tokens`、未知模型窗口语义）；同步默认值与 `defaults_are_bounded` 类测试 |
| CI、版本、安装包、tag | `.github/workflows/`、`Cargo.toml` | [Release](.agents/guides/release.md) |

指南与源码不一致时以源码为准，并在同一改动中更新该指南；一个事实只归属根文档或一个专题。

## 架构与全局不变量

```text
protium-core
  AppService -> AppHandle -> Engine
       |          |           |
       |          +-> protocol::AppSnapshotV2 / MessagePage
       |          +-> EventBridge::Envelope
       |
       +-- TUI adapter      -> Ratatui/Crossterm
       +-- WebUI adapter    -> REST/SSE
       +-- Desktop adapter  -> native IPC
```

- 核心独占 `SessionRuntime`/`AgentRunner`/Provider 与密钥/ToolRegistry 与 Security/Storage(SQLite)/审批 oneshot，以及命令串行队列与取消、关停逻辑；消费端不得触碰以上任何一项。
- 消费端（TUI/WebUI/Desktop）只允许使用 `AppService::start(CoreConfig)`、`AppHandle` 的 snapshot/messages/submit/execute_command/approve/cancel/activate/set_provider/subscribe/shutdown 接口，以及 `protocol.rs` 的 DTO 与 `bridge.rs` 的原子订阅（`subscribe_from`/`ResyncRequired`）。`submit` 返回请求序号、`cancel` 携带之防陈旧取消。
- 消费端不解析 `AgentEvent` 或私有 JSON，只消费 `Envelope/Event`；Provider 私有协议先规范化为 `ModelEvent`，再经 protocol 映射给消费端。
- 启动先取 snapshot，再 `subscribe_from(event_cursor)` 原子订阅并按 cursor 去重；游标逐出（`ResyncRequired`）或消费者滞后必须 resync（重取 snapshot + 消息页）。
- TUI 是消费端 adapter：`App` 持有 `AppHandle` 与 `TuiSessionProjection`，经命令队列提交全部变更，不直接触碰核心；核心引擎独占会话状态，切换不停止后台任务；前端退出/断连不等于取消 agent。
- 恢复沿 `head_turn_id` 父链；fork 不复制 Provider 服务端状态；undo/redo 移动 head 并按 `file_snapshots` 回滚/前滚文件（无快照的路径跳过）。
- workspace 必须 canonicalize；拒绝绝对路径、`..`、符号链接逃逸；新目标验证 canonical parent。
- Web 每次重定向都校验 HTTP/HTTPS 和公网地址；危险操作始终经过 mode、安全分类与审批；审批可"本会话放行"（进程内不落盘，config deny 仍压过它）。
- API Key 只来自环境变量或系统钥匙串，不进入 TOML、SQLite、日志、导出或模型上下文。
- 外部进程必须支持超时、输出截断、取消和进程树清理；`Esc` 产生可观察终态。
- 新增容量或并发前定义硬上限、截断、取消与释放；未知模型使用显式窗口或 Provider 感知注册表。

## 实施与验证

| 改动 | 最小验证 |
| --- | --- |
| 文档 | `bash scripts/check-agent-docs.sh`、`git diff --check` |
| 迭代中 | `cargo test --lib --all-features --locked <filter>`；每次只选一个相关过滤器 |
| 局部 Rust 完成 | `cargo fmt --all -- --check`、`cargo test --lib --all-features --locked` |
| 工具/存储/安全/进程或跨模块 | `cargo clippy --all-targets --all-features --locked -- -D warnings`、`cargo test --workspace --all-features --locked` |
| 发布 | 读取 Release 专题并运行其完整验证 |

保持改动聚焦，复用现有 helper，不清理无法证明无用的文件。未运行的检查必须在最终回复说明；不要因 Cargo 锁或冷缓存终止正常构建。验证档位按改动面判定：纯文档/展示改动可不跑 clippy 与全量；协议适配必须覆盖 match、回放与 projection；跨模块才升级全量。核心源码及其测试只在 `1H-Agent-core` 仓库运行。
