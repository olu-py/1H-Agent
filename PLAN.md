# 将 WebUI 后端核心迁移到旧 TUI 项目

## 总体方案

把 `1H-Agent-webUI` 中的 `crates/protium-core` 迁入当前仓库，保留旧 TUI 的 Ratatui/Crossterm 渲染层。最终形成：

```text
1h-agent-tui
  ├─ TUI 输入、布局、主题、Markdown、鼠标交互
  └─ TuiAdapter
       └─ protium-core::AppHandle
            ├─ AgentRunner
            ├─ Provider
            ├─ ToolRegistry / Security
            ├─ SessionRuntime
            ├─ SQLite/WAL Storage
            └─ EventBridge + v2 Protocol
```

TUI 与核心使用进程内 `AppHandle` 和 `protocol::Event` 连接，不启动 Web 服务、不通过 HTTP 回环调用、不保留第二套 Agent 状态机。`web/`、`crates/1h-agent-web`、Axum、SSE、RustEmbed 和 Web 鉴权代码不迁移。

## 代码组织

- 将新项目的 `crates/protium-core/src` 迁入当前仓库的 `crates/protium-core`，包含：
  - `agent.rs`、`app.rs`、`commands.rs`、`config.rs`
  - `provider/`、`tools/`、`security.rs`
  - `storage.rs`、`session.rs`、`secrets.rs`
  - `model.rs`、`input.rs`、`settings.rs`
  - 新增的 `protocol.rs`、`bridge.rs`、`service.rs`
- 根项目改为 Cargo workspace；旧 TUI 作为 `1h-agent-tui` 包，依赖 `protium-core`。
- 旧 TUI 保留 `home.rs`、`ui.rs`、`output.rs`、`ui_layout.rs`、`ui_theme.rs`、`ui_view_model.rs`、`clipboard.rs` 以及 Ratatui/Crossterm 依赖。
- TUI 不再直接拥有 `AgentRunner`、`Storage`、`ToolRegistry`、Provider 请求、审批 oneshot 或 `router_rx`。
- 旧 `src/app.rs` 改造成 TUI 门面，建议保留 `App` 名称以减少渲染代码改动，但其内部只保存：
  - `AppHandle`
  - 当前 `TuiSessionProjection`
  - TUI 专属输入、菜单、滚动、布局和鼠标状态
  - 最近会话及子 Agent 展示状态

## v2 协议接入

TUI 启动流程改为：

1. canonicalize workspace。
2. 读取共享 `Config`。
3. 构造 `CoreConfig`，调用 `AppService::start`。
4. 调用 `AppHandle::snapshot()` 获取：
   - 当前会话
   - 会话列表
   - provider/model/mode
   - busy/phase/status
   - pending approval
   - todo
5. 用 `snapshot.event_cursor` 建立 `replay_after + subscribe` 事件消费。
6. 对当前会话调用 `messages()`，初始化 TUI 展示投影。
7. 进入原有 Ratatui 事件循环。

所有上行操作统一调用 `AppHandle`：

| TUI 行为 | 核心调用 |
| --- | --- |
| 首条输入/普通输入 | `submit(session_id, text)` |
| `/undo`、`/redo`、`/fork`、`/todo` 等 | `execute_command(session_id, text)` |
| Esc 取消 | `cancel(session_id)` |
| 会话切换 | `activate_session(session_id)` |
| 审批 y/n/a/Esc | `approve(approval_id, accept)` |
| Provider/model 更新 | `set_provider(...)` 或扩展后的非密钥配置接口 |
| 退出 | `shutdown()` |

禁止 TUI 重新实现 `Command` 解析、审批等待、取消 abort、会话删除和文件回滚。

## TUI 状态投影

旧渲染器依赖 `SessionRuntime` 的 `entries`、thinking、tool、approval 和滚动字段，而 v2 核心不向消费端暴露内部 runtime。因此新增 TUI 专属投影层：

- `MessageDto -> DisplayEntry`：用于恢复历史消息。
- `Event` reducer：处理 `text_delta`、`reasoning_delta`、`tool_started`、`tool_finished`、`todo_updated`、`child_session_progress`、`completed`、`failed`、`cancelled` 等事件。
- `Approval` 只保存 `approval_id`、工具调用和原因，不保存 oneshot sender。
- `Completed`、`transcript_invalidated`、`resync_required` 后重新调用 `messages()`，以数据库结果重建历史，避免 TUI 自己推断持久化状态。
- 流式文本和思考内容在本地投影中增量更新，保持旧 TUI 的实时渲染体验。
- 未知 v2 事件静默忽略，并记录调试日志。
- `Event` 与 Crossterm 的 `Event` 使用显式别名，避免类型冲突。

旧 `ui.rs` 尽量继续读取兼容字段；无法直接复用的核心字段迁移到 `TuiSessionProjection`，不向 core 暴露 TUI 布局字段。

## 需要补齐的核心接口

新项目现有 `AppHandle` 已覆盖大部分行为，但为保证旧 TUI 功能完整，需要补齐：

- 非密钥 Provider 配置更新接口，覆盖旧设置页实际修改的字段；API Key 仍只能写入环境变量/系统钥匙串。
- thinking level/budget 的配置接口，或将其纳入同一个非密钥 Provider 更新请求。
- 一个明确的 `subscribe_from(cursor)` 辅助方法，封装回放、订阅和 resync 语义，避免每个消费端重复实现。
- 如 TUI 需要展示当前审批之外的后台审批，提供按 session 的审批查询或保持现有全局最早审批语义并在 TUI 中明确显示来源会话。

这些接口必须继续通过核心命令队列串行执行，不能让 TUI 直接修改 core 内部 `Config`、`SessionRuntime` 或 `PendingApproval`。

## 会话首页和配置兼容

- 首页最近会话来自 `snapshot.sessions`，不再直接打开 SQLite。
- 新会话采用 `submit(None, first_prompt)`，由核心创建会话；收到 `sessions_changed` 后刷新 snapshot 并进入主界面。
- 保持现有 `data_dir/agent.db` 路径，直接复用新核心的 SQLite migration、head 链、file snapshot、undo/redo 逻辑。
- 首次迁移前验证旧数据库备份、会话树、消息、provider 状态和文件快照。
- 保留旧配置键的读取兼容；Web 专属 `server` 字段即使暂时保留，也只能由 Web adapter 使用，core 不读取 bind/port/auth。
- 保留 `1h-agent` 命令作为兼容别名，同时将正式二进制命名为 `1h-agent-tui`。

## 实施阶段

1. **Workspace 与核心导入**
   - 建立 workspace 和 `protium-core` crate。
   - 迁入新核心源码、依赖和配置迁移。
   - 先让 core 单独通过 fmt、lib tests、Clippy。

2. **TUI 启动适配**
   - 用 `AppService::start` 替换旧 `build_app`/直接 Storage 初始化。
   - 删除 TUI 内部 AgentRunner、provider 请求和 router channel。
   - 建立 snapshot/messages/bootstrap 流程。

3. **事件和命令适配**
   - 增加 TUI projection reducer。
   - 替换输入、命令、审批、取消、切换会话、provider 设置调用。
   - 保持原有键盘、鼠标、菜单和渲染行为。

4. **功能 parity**
   - 覆盖流式回复、思考、工具卡片、审批、取消、todo、子 Agent、mode、fork、undo/redo、diff、export、compact。
   - 重点验证删除会话、后台 runtime 淘汰和退出清理只由 core 执行一次。

5. **清理和发布**
   - 删除 TUI 中重复的后端实现和无效依赖。
   - 更新 README、Cargo metadata、安装脚本和维护指南。
   - 保留 WebUI 项目作为未来独立 adapter，不把前端资源放入 TUI 二进制。

## 验证标准

- Core：`cargo fmt --all -- --check`、完整 lib tests、`cargo clippy --all-targets --all-features --locked -- -D warnings`。
- TUI：Ratatui `TestBackend` 覆盖首页、消息流、工具状态、审批、取消、会话切换和 resync。
- 集成测试：mock provider 下验证 `AppService -> AppHandle -> EventBridge -> TUI projection` 全链路。
- 持久化测试：旧数据库迁移、10,000 条消息分页、head 父链、fork、undo/redo 文件快照。
- 生命周期测试：workspace 独占锁、审批超时、Esc 取消终态、删除子树、后台 runtime 关停、`shutdown` 释放资源。
- 手工验收：首条消息建会话、流式 reasoning/text、工具审批批准/拒绝、取消、provider/model 切换、集群子 Agent、重启恢复。
- 最终执行 `cargo test --all-features --locked`、`git diff --check` 和 `bash scripts/check-agent-docs.sh`。

## 默认假设

- TUI 与核心在同一进程内运行，通过 `AppHandle`/v2 DTO/EventBridge 连接。
- 不引入常驻 daemon、跨进程附着、TUI 到 Web 服务的 HTTP 调用。
- 本次迁移只实现“新核心 + 旧 TUI”；Web HTTP/SSE adapter 和 React 前端继续留在 `1H-Agent-webUI`，以后可基于同一个 `protium-core` 独立接入。
- v2 协议作为唯一新接口，旧 TUI 内部事件链逐步移除，不维护 v1/v2 双状态机。
