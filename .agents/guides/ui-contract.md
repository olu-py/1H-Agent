# 通用 UI 契约维护指南

## 适用范围

所有 TUI/WebUI/Desktop 消费端 adapter 的接入约束：经 `AppHandle` 提交变更、消费 `Envelope/Event`、处理事件游标回放与 resync、遵守协议加法演进。

## 入口

- `crates/protium-core/src/service.rs`：`AppService::start`、`AppHandle` 全部类型化接口。
- `crates/protium-core/src/protocol.rs`：v2 DTO 权威定义（`Event`/`AppSnapshotV2`/`MessageDto`/`MessagePage`/`Envelope`）。
- `crates/protium-core/src/bridge.rs`：`EventBridge` 事件游标/回放（`replay_after`/`ReplayResult`）。
- 各消费端 adapter 的 transport/projection/render 层。

## 不变量

- 所有变更经 `AppHandle` 的 `CoreCommand` 队列串行进入 Engine；消费端不得触碰 `SessionRuntime`/`AgentRunner`/Storage/Provider/审批 oneshot。
- 消费端只消费 `Envelope/Event`，不解析 `AgentEvent` 或 Provider 私有 JSON；Provider 事件先规范化为 `ModelEvent` 再映射进 protocol。
- 启动先取 snapshot（`AppSnapshotV2`），按 `event_cursor` replay 后再 subscribe；游标逐出（`ResyncRequired`）或消费者滞后时必须重取 snapshot + 消息页 resync。普通 snapshot 刷新不得推进未消费 cursor，只有首次连接、滞后或 `ResyncRequired` 才建立新基线。
- 消息经 `MessagePage` 游标（`next_before`/`has_more`）分页；Approval 只传 `approval_id`，不得跨接口暴露 oneshot sender。消费端收到 live `Approval` 必须立即展示，收到 `ApprovalResolved` 必须关闭匹配项并从 snapshot 收敛全局下一个审批，不得等待其他事件或服务端超时。
- 思考/正文阶段序：同一模型轮次内事件顺序保证为 `ReasoningDelta* -> ReasoningCompleted -> TextDelta*`。`ReasoningCompleted` 无负载，只在产生思考时恰好发一次（无思考的轮次不发），且是该轮思考视图到正文视图的渲染屏障：消费端必须在收到它时提交/刷新思考摘要并结束 live 思考行，再接受后续正文 delta。旧客户端忽略该事件后收到的仍是同样的 delta 序列（加法兼容）；新客户端借此把"只有思考摘要"与"摘要+正文"分成两帧展示。
- v2 之后协议只做加法演进：新变体/字段必须被旧 UI 忽略，不得改名、重排或复用旧 tag。
- 新事件必须贯通 agent forward、session reducer、protocol mapping、bridge 与所有消费端展示；未知事件静默忽略。
- 分层约束：TUI 管 Ratatui 状态/projection/渲染/输入；WebUI 管 HTTP/SSE transport 与浏览器状态；Desktop 管 IPC transport 与原生窗口生命周期。transport 不承载业务规则，核心不依赖任何 UI 框架。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 事件丢失或乱序 | cursor -> replay_after -> subscribe 时序 -> 重复 subscribe |
| 新增事件不显示 | agent forward -> session reducer -> protocol mapping -> bridge -> 消费端 handler |
| 报 ResyncRequired | 桥接容量/字节上限 -> 消费者滞后 -> resync 路径 |
| 多消费端并发命令错乱 | `CoreCommand` 串行队列 -> 是否绕过 `AppHandle` 直改核心 |

## 验证

- 文档改动跑 `bash scripts/check-agent-docs.sh` + `git diff --check`。
- 新增协议事件：确认加法演进（旧 UI 可忽略）、贯通全部环节，并同步各消费端映射与展示。
- 协议/桥接改动升级到 `cargo test --lib --all-features --locked` 与完整 Clippy。
