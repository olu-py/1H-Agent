# 重构 Agent 维护文档以支持多前端架构

## Summary

将维护协议统一调整为“`protium-core` 独立演进，TUI、WebUI、原生桌面作为消费端适配器”的架构。文档明确核心状态机、通用协议和各前端职责边界，禁止任何前端直接操作 `SessionRuntime`、`AgentRunner`、Storage、Provider 或审批 oneshot。

本次只修改 `AGENTS.md`、`.agents/guides/` 和文档检查脚本，不修改 Rust 或前端实现。保留当前 TUI 专题，同时新增通用 UI 契约专题。

## 主要修改

### `AGENTS.md`

- 更新稳定上下文：
  - 项目由 `protium-core`、TUI adapter、WebUI adapter、Desktop adapter 组成。
  - 后端核心可独立发布和迭代。
  - TUI/WebUI/Desktop 只能通过通用接口连接核心。
  - 当前仓库保留单进程 TUI 运行方式，但文档不再把 TUI 视为核心状态机。
- 更新架构图：

```text
protium-core
  AppService -> AppHandle -> Engine
       |          |           |
       |          +-> protocol::Snapshot / MessagePage
       |          +-> EventBridge::Envelope
       |
       +-- TUI adapter      -> Ratatui/Crossterm
       +-- WebUI adapter    -> REST/SSE
       +-- Desktop adapter  -> native IPC
```

- 明确核心独占：
  - `SessionRuntime`
  - `AgentRunner`
  - Provider/密钥
  - ToolRegistry/Security
  - Storage/SQLite
  - 审批 oneshot
  - 命令串行队列和取消/关停逻辑
- 明确消费端只允许使用：
  - `AppService::start(CoreConfig)`
  - `AppHandle` 的 snapshot/messages/submit/execute_command/approve/cancel/activate/set_provider/subscribe/shutdown 等接口
  - `protocol.rs` 的 DTO 和 `bridge.rs` 的事件游标/回放
- 更新任务路由，新增 UI 通用接口路由，并将 Provider、Storage、Cluster、Tools、Runtime 的入口统一指向 `crates/protium-core`。
- 保留 TUI 路由，但说明 `src/app.rs` 是 TUI 门面，不是业务核心。
- 将 WebUI、Desktop 从“排除领域”改为“同一协议的外部消费端”；当前仓库不实现其 UI 代码，但维护协议覆盖其接入约束。

### 新增 `.agents/guides/ui-contract.md`

控制在检查脚本允许的 50 行以内，包含：

- 适用范围：所有 TUI/WebUI/Desktop 消费端。
- 权威入口：
  - `crates/protium-core/src/service.rs`
  - `crates/protium-core/src/protocol.rs`
  - `crates/protium-core/src/bridge.rs`
  - 各消费端 adapter 的 transport/projection/render 层
- 核心接口契约：
  - 所有变更经 `AppHandle` 串行进入 Engine。
  - 消费端不解析 `AgentEvent`，只消费 `Envelope/Event`。
  - 启动先取 snapshot，再按 `event_cursor` replay 后 subscribe。
  - 游标逐出或消费者滞后时必须 resync。
  - 消息通过 `MessagePage` 游标分页。
  - Approval 只传 `approval_id`，不得跨接口暴露 oneshot sender。
- 协议演进：
  - v2 之后只允许加法演进。
  - 新事件必须贯通 agent forward、session reducer、protocol mapping、bridge 和所有消费端。
  - 未知事件必须静默忽略。
- 分层约束：
  - TUI：Ratatui 状态、projection、渲染和输入。
  - WebUI：HTTP/SSE transport 和浏览器状态。
  - Desktop：IPC transport 和原生窗口生命周期。
  - transport 不承载业务规则，核心不依赖任何 UI 框架。

### 现有专题指南

- `runtime.md`
  - 将生命周期主体改为 core `Engine/AppHandle`。
  - 说明所有消费端共享同一核心状态机，前端退出/断连不等于取消 agent。
  - 将事件链改为 `Agent task -> Engine -> EventBridge -> adapter`。
  - 保留后台容量、删除子树、审批拒绝、取消、shutdown 和 workspace lock 规则。
  - 新增 adapter 断开、resync、重复订阅和多消费端并发命令诊断项。

- `tui.md`
  - 明确 TUI 只负责 projection、输入、布局、渲染、滚动和复制。
  - 所有会话/命令/审批/取消/provider 操作都调用 `AppHandle`。
  - `TuiSessionProjection` 只保存展示状态，不保存核心 runtime 或 oneshot。
  - 终态、`transcript_invalidated`、`resync_required` 时从 snapshot/messages 重建展示状态。
  - 保留现有 Ratatui 性能、缓存、hit-test、重绘和 UTF-8 约束。

- `provider.md`
  - 将 Provider 配置、密钥、协议解析、重试和恢复明确归入 core。
  - 消费端不能读取 API Key、直接构造 Provider 请求或解析私有 JSON。
  - 前端只能通过非密钥配置接口切换 provider/model/thinking。
  - Provider 事件必须先规范化为公共 `ModelEvent`，再经 protocol 映射给消费端。

- `cluster.md`
  - 将集群调度、审批、取消、预算、子会话持久化归入 core。
  - TUI/WebUI/Desktop 只能展示 `ChildSessionProgress` 等公共事件。
  - 各消费端不得自行调度子 Agent、维护审批 owner 或复制批次状态机。

- `tools.md`
  - 将工具定义、权限分类、路径安全、进程控制、SSRF 校验归入 core。
  - UI 只负责工具卡片、风险说明和审批交互；不得重新判断权限。
  - 工具新增时增加 protocol 事件/DTO 与各消费端展示映射检查。

- `storage.md`
  - 明确 SQLite/WAL、会话树、消息、provider state、file snapshots 全部由 core 独占。
  - 消费端不得直接打开数据库或绕过 `AppHandle` 查询/写入。
  - `snapshot/messages` 是消费端读取历史的唯一入口。
  - 保留迁移、head 父链、fork、undo/redo、软删和快照上限规则。

- `release.md`
  - 增加 workspace/core/adapter 的版本兼容要求。
  - 核心协议版本与消费端兼容性必须在发布前验证。
  - 分别验证 TUI、WebUI、Desktop adapter；核心变更需覆盖所有消费端。
  - 继续保留平台归档、checksums、安装包和完整 Rust 验证矩阵。

### `scripts/check-agent-docs.sh`

- 将 `ui-contract` 加入固定指南清单。
- 保持每个指南必须包含五个标准章节。
- 保持根文档每个指南只路由一次。
- 保持根文档和指南行数上限；如新增架构说明导致超限，压缩重复叙述而不是放宽检查上限。

## Test Plan

- `bash scripts/check-agent-docs.sh`
- `git diff --check`
- 用 `rg` 检查旧的“WebUI 完全排除”“TUI 直接拥有 SessionRuntime”“消费端解析 AgentEvent”等矛盾表述已移除。
- 检查所有指南入口是否与目标架构一致：
  - core 代码只出现在 core/协议专题；
  - TUI 代码只出现在 TUI 专题；
  - WebUI/Desktop 只作为 adapter 和契约消费端出现。
- 不运行 Cargo 测试，因为本次不修改 Rust、Cargo 或协议实现。

## Assumptions

- `protium-core` 是后端核心的唯一业务状态机，未来可以独立迭代。
- TUI、WebUI、Desktop 可以有不同 transport，但必须共享同一 `AppHandle` 语义和 v2 DTO/Event 契约。
- 当前仓库暂不新增 WebUI 或 Desktop 源码；文档只规定它们的接入边界。
- 当前工作树中用户已有的 `PLAN.md` 删除改动保持不变，不在本次文档修改中恢复或覆盖。
