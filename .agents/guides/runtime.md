# Runtime 生命周期与取消维护指南

## 适用范围

`App` 的当前/后台 runtime 停放与容量、会话切换、删除子树关停、退出清理、agent 任务取消与审批拒绝链路。

## 入口

- `crates/protium-core/src/app.rs`：核心 `App` 的 `activate_session`、`evict_background_overflow`、`handle_routed_event`、router 事件循环收尾。
- `crates/protium-core/src/session.rs`：`SessionRuntime` 的 `shutdown`/`idle`/`parked_at`、终态事件复位。
- `crates/protium-core/src/storage.rs`：`delete_session` 返回被删子树全部 id。
- `crates/protium-core/src/service.rs`：`AppHandle` 类型化操作与 `CoreCommand` 串行化；TUI 门面 `src/app.rs` 只经 `AppHandle` 变更、从 EventBridge 消费 `Envelope`。

## 不变量

- 事件链三段：agent task --`agent_tx`--> 无句柄转发 task --`router_tx`--> `App.router_rx`。`agent_rx` 归转发 task，runtime drop 不关闭通道；事件按 `session_id` 路由，未知 id 静默丢弃。
- drop `JoinHandle` 不取消 tokio 任务；只有显式 `abort()` 才终止。子 Agent 跑在父任务同一棵 future 树里，abort 父任务即级联终止。
- `shutdown()` 必须先拒绝未决审批（oneshot `send(false)`）再 abort：abort 会 drop agent 持有的 receiver，后发必失败。
- `Completed`/`Failed`/`Cancelled`/`LocalCommandFinished` 终态复位 `busy`/`active_task`；非终态事件不得复位。`Esc` 仅作用于当前会话。
- 后台总量硬上限 `runtime.max_background_sessions`（clamp 2..=64，默认 8）：超限优先 LRU 淘汰空闲项，全忙时关停最旧项；当前会话不计入。淘汰后切回走 `build_runtime` 从存储重建。
- `/delete` 软删整个子树（含后代）并按返回 id 关停全部对应 runtime、拒绝其审批、清理跟踪表；删除最后一个会话时新建替代会话。
- TUI 门面不拥有 `SessionRuntime`/router/审批 oneshot：所有变更经 `AppHandle` 的 `CoreCommand` 队列串行，事件经 bridge `Envelope` 回流；`Esc`/审批走 `AppHandle::cancel`/`approve`。门面 `sync_from_snapshot` 把 `child_status`/`child_batches`/`expanded_sessions` 收敛到快照中的活会话集合，吸收迟到事件造成的再污染。
- undo/redo 移动 head 后按 `file_snapshots` 回滚/前滚目标文件（undo 写 pre_image、redo 写 post_image；无快照的路径跳过不误伤）。快照上限：单文件 `checkpoint_max_file_bytes`（clamp 4KB..=8MB，超限记 marker 提示"未回滚"）、单会话 `checkpoint_max_session_bytes`（clamp 1MB..=256MB，超限丢最旧）；回滚 IO 失败不阻断会话恢复，只在 status 提示。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 删除后任务仍跑 | `deleted_ids` 覆盖 -> `shutdown` 调用 -> abort 顺序 |
| 删除后审批悬空 | oneshot 拒绝先于 abort -> owner 路由 |
| 后台内存增长 | 容量配置 -> 淘汰触发点（切换/终态事件） -> 全忙关停 |
| 切回会话丢流式状态 | 是否被淘汰 -> `build_runtime` 重建路径 |
| 面板残留子会话 | `refresh_sessions` 收敛 -> `child_batches`/`child_status` 清理 |

## 验证

- 迭代过滤器：`delete_`、`background_capacity`、`switching_session`、`handle_routed_event`。
- 生命周期、容量或取消协议变更升级到完整测试和 Clippy。
- 新增 `AgentEvent` 变体需一次接通：agent 内 forward 闭包（`Forwarded::Send`/`SendIgnore`/`Ignore` 语义按需选）→ `session.rs handle_event` 穷尽 match → `app.rs` 路由/`should_coalesce_stream_redraw` 是否合并；压缩与子 agent 的 `|_| Ignore` 闭包自动忽略但行为要确认。
- app 层测试注意：`test_app` 构造的 `SessionRuntime` 含 tokio 组件，涉及审批/undo 的测试必须 `#[tokio::test]`；改 `resolve_approval` 等被测试直接调用的签名会连带旧调用点编译失败，改动时一并扫 `grep resolve_approval(`。
