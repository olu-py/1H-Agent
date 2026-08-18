# 1H-Agent AI 维护协议

> 读取对象：维护本仓库的编码 Agent。先读本文件，再按任务路由只读取相关专题和源码；不要一次扫描整个仓库。

## 0. 稳定上下文

```text
project: 1H-Agent
meaning: 1H = 氕（protium），不是“一小时”
goal: 极致轻量、高性能、权限感知的跨平台终端 Agent
runtime: 单个 Rust/Tokio 进程，Ratatui + Crossterm，SQLite/WAL
authority: 源码 > config/config.example.toml > .github/workflows > 本文件 > 专题指南
scope: TUI、模型流、受控工具、多会话、AI 集群、跨平台发布
excluded: Web UI、内置浏览器、远程 MCP、动态插件、图片和语音能力
```

不可违反：保持单 Rust 二进制；不引入 Node.js、Python、Chromium、Web UI、动态插件 ABI 或后台轮询。所有路径、网络、工具、进程、缓存、channel 和输出必须有边界、取消与释放路径。

## 1. 一分钟上手

1. 运行 `git status --short --branch`，识别用户已有改动；不得还原或覆盖无关文件。
2. 从 `src/main.rs -> app::run` 进入。全局状态在 `App`，单会话状态在 `SessionRuntime`，模型/工具循环在 `AgentRunner`。
3. 用 `rg` 搜索目标符号、事件变体和配置字段，只读取定义、直接调用者、相邻测试及一个相关专题。
4. 修改事件、配置或持久化类型时，搜索全部构造点、match 分支、序列化、恢复和测试。
5. 先跑最小目标测试；只有跨模块行为变化才升级到完整 Clippy 和测试。

核心文件：

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` / `src/app.rs` | 启动、全局 UI、事件循环、会话注册表和路由 |
| `src/session.rs` | 单会话对话、显示条目、流式状态、滚动和事件处理 |
| `src/agent.rs` | 模型循环、工具、审批、子 Agent 和集群批次 |
| `src/provider/openai.rs` | Chat/Responses 请求、SSE 和 Provider 事件规范化 |
| `src/tools/` / `src/security.rs` | 工具执行、权限、workspace 和网络边界 |
| `src/config.rs` / `src/settings.rs` / `src/secrets.rs` | 配置、Provider 档案、设置和钥匙串缓存 |
| `src/storage.rs` | SQLite 会话、turn/message、工具结果和 Provider 状态 |
| `src/ui.rs` / `src/output.rs` | Ratatui 渲染、布局缓存、命中、选择和复制 |

## 2. 任务路由

| 任务 | 首读源码 | 按需指南 |
| --- | --- | --- |
| Provider、模型、设置、密钥、HTTP 400 | `src/config.rs`、`src/agent.rs` | [Provider](.agents/guides/provider.md) |
| 集群、子 Agent、审批、取消、停滞 | `src/agent.rs`、`src/app.rs` | [Cluster](.agents/guides/cluster.md) |
| TUI、滚动、长文本、重绘、点击 | `src/app.rs`、`src/ui.rs` | [TUI](.agents/guides/tui.md) |
| 发布、版本、安装包、tag | `.github/workflows/release.yml` | [Release](.agents/guides/release.md) |
| 工具、路径、SSRF、进程 | `src/tools/`、`src/security.rs` | 本文件第 4 节 |
| 会话、分支、迁移、恢复 | `src/storage.rs`、`src/session.rs` | Provider 变更时再读 Provider 指南 |

每次任务只读取命中的指南；跨领域变更才组合读取。指南与源码不一致时以源码为准，并在同一改动中更新指南。

## 3. 架构与数据流

```text
terminal event -> App -> SessionRuntime -> AgentRunner -> OpenAiClient / ToolRegistry
                     |          |               |
                     +-> UI     +-> Storage     +-> RoutedEvent(session_id) -> App
```

- `App` 只维护全局 UI、当前/后台 runtime 和事件路由；`SessionRuntime` 拥有单会话状态，后台会话不因切换而停止。
- `provider` 将私有协议规范化为 `ModelEvent`；`tools` 执行受控能力；UI 不接触 Provider 私有 JSON。
- 跨模块优先使用 `ConversationItem`、`ModelRequest`、`ModelEvent`、`ToolCall`、`ToolDefinition` 和 `Usage`。
- 恢复会话沿 `head_turn_id` 父链读取可见历史；fork 不复制 Provider 服务端状态；undo/redo 只移动 head。
- `Esc` 必须取消当前活跃会话的模型、审批等待和外部进程，并产生可观察终态。

## 4. 全局安全与资源规则

- workspace 启动时 canonicalize；已有目标验证根内路径，新目标验证 canonical parent；拒绝绝对路径、`..` 和符号链接逃逸。
- Web 仅允许 HTTP/HTTPS；每次重定向都重新校验，拒绝 loopback、私网、链路本地、未指定和多播地址。
- 权限顺序：硬安全/mode -> 专用规则 -> `security::classify_tool` -> `[permissions.tools]`；危险操作始终审批。
- API Key 只来自 Provider 环境变量或系统钥匙串，不得进入 TOML、SQLite、日志、导出或模型上下文。
- Unix 进程使用独立进程组，Windows 清理进程树；新增进程必须支持超时、输出截断和取消。
- 新增缓存、channel、索引、上下文或并发前，先定义容量、截断、取消和释放；具体默认值以 `src/config.rs` 及相邻测试为准。
- 未知模型不得猜测上下文窗口；使用显式配置或 `src/config.rs` 的 Provider 感知注册表。

## 5. 实施与验证

1. 保持改动聚焦，复用现有模块和 helper；不顺手重构，不清理无法证明无用的用户文件。
2. 行为变更添加目标测试；跨平台进程、网络、存储和取消变更覆盖失败、超时与恢复。
3. 文档改动：`bash scripts/check-agent-docs.sh`、`git diff --check`。
4. Rust 局部改动：`cargo fmt --all -- --check` 加相关测试过滤器。
5. 跨模块或公共行为：`cargo clippy --all-targets --all-features --locked -- -D warnings`、`cargo test --all-features --locked`。
6. 发布影响：再运行 `cargo build --release --locked`，并按 Release 指南核验工作流与产物。

不要因 `target/` 冷缓存或 Cargo 构建锁而终止正常构建。未运行某项检查时，最终回复必须明确说明。

## 6. 高效诊断

- 卡顿：先看 channel send、逐事件 draw、重复布局/Markdown 解析、后台事件误重绘，再看模型或工具速度。
- Future 不结束：依次检查 queue、semaphore、stream、tool future、approval lock、oneshot 和 Drop guard 的释放与终态。
- 配置丢失：检查迁移、当前激活副本、session provider/model、runner rebuild 和持久化，不要用预设默认值掩盖问题。
- 点击错位：从实际裁剪后的 layout 计算坐标，分别测试文字、分隔符、菜单和滚动偏移，不硬编码列数。
- 清理目录：先用 `git status --ignored` 与 `du` 证明目标是生成物；数据库、配置、release notes、源码和用户改动不可猜测删除。

## 7. 维护检查

- 新能力是否事件驱动、空闲不重绘、所有增长有硬上限？
- 新工具是否经过注册、workspace/SSRF、mode、审批、超时和进程清理？
- 新 Provider 事件是否先规范化，并定义持久化、重放和恢复语义？
- 新文档事实是否只在一个权威位置出现，并可回溯到源码、配置、测试或工作流？
