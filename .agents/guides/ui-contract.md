# 通用 UI 契约维护指南

## 适用范围

所有 TUI/WebUI/Desktop 消费端 adapter 的接入约束：经 `AppHandle` 提交变更、消费 `Envelope/Event`、处理事件游标回放与 resync、遵守协议加法演进。

## 入口

- 独立 core 仓库 `src/service.rs`：`AppService::start`、`AppHandle` 全部类型化接口。
- `protium-core (Git dependency): src/protocol.rs`：v2 DTO 权威定义（`Event`/`AppSnapshotV2`/`MessageDto`/`MessagePage`/`Envelope`/`ContextBudgetDto`/`PartialDto`）。
- `protium-core (Git dependency): src/bridge.rs`：`EventBridge` 原子订阅（`subscribe_from`/`Subscription`/`ResyncRequired`）。
- `protium-core (Git dependency): src/conformance.rs`（`test-util` feature，非默认）：共享回放场景、`check_stream_invariants` 与穷尽变体目录；JSON 夹具产物在 `protium-core (Git dependency): conformance/` 供外部消费端（WebUI 仓库）直接使用。
- 各消费端 adapter 的 transport/projection/render 层。

## 不变量

- 所有变更经 `AppHandle` 的 `CoreCommand` 队列串行进入 Engine；消费端不得触碰 `SessionRuntime`/`AgentRunner`/Storage/Provider/审批 oneshot。`submit` 返回请求序号、`cancel` 须携带之（序号不匹配则静默忽略），防陈旧取消误杀新请求。
- 消费端只消费 `Envelope/Event`，不解析 `AgentEvent` 或 Provider 私有 JSON；Provider 事件先规范化为 `ModelEvent` 再映射进 protocol。
- 启动先取 snapshot，再 `subscribe_from(event_cursor)` 原子订阅（先订 live 再快照 ring，replay∩live 重叠按 cursor 去重，跳过 `cursor <= 已处理` 的 live 事件）；`ResyncRequired`（游标逐出）或消费者滞后时重取 snapshot + 消息页并重新订阅。普通 snapshot 刷新不得推进未消费 cursor，只有首次连接、滞后或 `ResyncRequired` 才建立新基线。
- 消息经 `MessagePage` 游标（`next_before`/`has_more`）分页；Approval 只传 `approval_id`，不得跨接口暴露 oneshot sender。消费端收到 live `Approval` 必须立即展示，收到 `ApprovalResolved` 必须关闭匹配项并从 snapshot 收敛全局下一个审批，不得等待其他事件或服务端超时。
- 思考/正文阶段序：同一模型轮次内事件顺序保证为 `ReasoningDelta* -> ReasoningCompleted -> TextDelta* -> ToolCallStreaming* -> Approval/ToolStarted`。`ToolCallStreaming`（工具参数流式进度：`name` 可选、`received_bytes` 单调、事件有序且有界，约 1 KiB 阈值合并为核心实现细节——消费端只可依赖单调/有序/有界，不可依赖具体阈值或事件数）紧跟最后 `TextDelta`、先于审批/工具开始，供消费端显示"生成工具调用"动画行而避免静默冻结；旧客户端忽略它后收到的仍是同样的 delta 序列。`ReasoningCompleted` 无负载，只在产生思考时恰好发一次（无思考的轮次不发），且是该轮思考视图到正文视图的渲染屏障：消费端必须在收到它时提交/刷新思考摘要并结束 live 思考行，再接受后续正文 delta。新客户端借此把"只有思考摘要"与"摘要+正文"分成两帧展示。
- v2 之后协议只做加法演进：新变体/字段必须被旧 UI 忽略，不得改名、重排或复用旧 tag。
- 事件接线链单一事实源：`ModelEvent`（provider 归一化）→ `AgentEvent`（forward 闭包，可 `SendMany` 展开为有序序列，如 `ReasoningCompleted`+`TextDelta`）→ protocol `Event`（session reducer 映射）→ `bridge` → 各消费端展示映射；沿链贯通，未知事件静默忽略。各专题只保留本层细节并回指本指南。
- 分层约束：TUI 管 Ratatui 状态/projection/渲染/输入；WebUI 管 HTTP/SSE transport 与浏览器状态；Desktop 管 IPC transport 与原生窗口生命周期。transport 不承载业务规则，核心不依赖任何 UI 框架。
- core/TUI 同步开发可用 Cargo `--config` path patch 指向同级独立 clone；不得改 manifest 或提交 path 锁文件，正式验收必须恢复 Git source。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 事件丢失或乱序 | cursor -> `subscribe_from` 时序（先订 live 再快照 ring）-> 按 cursor 去重 -> 重复订阅 |
| 新增事件不显示 | agent forward -> session reducer -> protocol mapping -> bridge -> 消费端 handler |
| 生成工具参数期间屏幕静默无反馈（用户感知"思考没显示、审批后立刻完成"） | provider 归一化 -> agent forward 合并/阈值 -> protocol 映射 -> bridge -> 消费端 handler（`ToolCallStreaming` 链路）；本地写文件毫秒级完成属正常 |
| 报 ResyncRequired | 桥接容量/字节上限 -> 消费者滞后 -> resync 路径 |
| 多消费端并发命令错乱 | `CoreCommand` 串行队列 -> 是否绕过 `AppHandle` 直改核心 |

## 验证

- 文档改动跑 `bash scripts/check-agent-docs.sh` + `git diff --check`。
- 新增协议事件：确认加法演进（旧 UI 可忽略）、贯通全部环节，并同步各消费端映射与展示。
- 新增协议事件的强制链路：`conformance.rs` 穷尽变体目录先编译失败 -> 补至少一个场景 -> `cargo test --lib --features test-util conformance`（场景不变量 + 契约测试 `scripted_round_emits_documented_order` + JSON 夹具无漂移并提交）-> TUI 回放测试（`conformance_corpus_replays_into_projection`、`conformance_corpus_replays_and_dedups_by_cursor`）自动消费新场景。夹具仅在 `test-util` feature 下编译，不进 release 二进制。
- 协议/DTO 先在 core 仓库更新 conformance 与 `bindings/` 并 push；本仓库再执行 `cargo update -p protium-core` 和 TUI conformance 回放。TUI 不复制 TS bindings，WebUI 在其独立仓库同步。
- 本地 patch 联调结束后检查 metadata source 为 Git、`Cargo.lock` 为预期 SHA，再运行全部 `--locked` 验证。
- 事件/协议变更端到端速查：`ModelEvent`（provider 归一化）→ `AgentEvent`（forward 闭包合并/阈值，如 `ToolCallStreaming` 1 KiB 合并）→ `protocol::Event`（`routed_to_event` 映射 + doc 顺序保证）→ `bridge::event_payload_bytes` → 核心 reducer（`session.rs` phase/status）→ 消费端展示（TUI 见 tui.md）→ bindings（`cargo test` 导出测试）→ 测试（agent scripted / conformance 契约与回放 / protocol serde / projection / facade 帧文本）。
- 协议/桥接改动升级到 `cargo test --lib --all-features --locked` 与完整 Clippy。
