# Runtime 生命周期与取消维护指南

## 适用范围

core `Engine`/`AppHandle` 生命周期主体：当前/后台 runtime 停放与容量、会话切换、删除子树关停、退出清理、agent 任务取消与审批拒绝链路，及消费端 adapter 的事件消费契约。

## 入口

- `protium-core (Git dependency): src/service.rs`：`Engine`/`AppHandle`、`CoreCommand` 串行队列、`AppService::start` 的 workspace 独占锁与 `shutdown`。
- `protium-core (Git dependency): src/app.rs`：核心 `App` 的 `activate_session`、`evict_background_overflow`、`handle_routed_event`。
- `protium-core (Git dependency): src/session.rs`：`SessionRuntime` 的 `shutdown`/`idle`/`parked_at`、终态事件复位。
- `protium-core (Git dependency): src/storage.rs`：`delete_session` 返回被删子树全部 id。
- `protium-core (Git dependency): src/bridge.rs`：`EventBridge` 向所有消费端 fan-out `Envelope`。

## 不变量

- 所有消费端共享同一核心状态机；前端退出/断连不等于取消 agent，adapter 重连后走 snapshot + replay 恢复。
- 事件链：agent task -> `Engine` -> `EventBridge` -> adapter；`AppHandle` 是唯一变更入口，命令经 `CoreCommand` 串行。
- drop `JoinHandle` 不取消 tokio 任务；只有显式 `abort()` 才终止。子 Agent 跑在父任务同一棵 future 树里，abort 父任务即级联终止。
- `shutdown()` 必须先拒绝未决审批（oneshot `send(false)`）再 abort：abort 会 drop agent 持有的 receiver，后发必失败。
- `Completed`/`Failed`/`Cancelled`/`LocalCommandFinished` 终态复位 `busy`/`active_task`；非终态事件不得复位。`Esc` 仅作用于当前会话。
- `submit` 返回请求序号（`request_seq`，会话内单调递增）；`cancel(session, request_seq)` 只有序号仍匹配当前请求才生效，陈旧取消静默忽略、绝不误杀新请求。会话 busy/有未决审批/无 runner 时提交返回结构化 `Conflict`，消费端保留输入文本由用户重试。
- 后台总量硬上限 `runtime.max_background_sessions`（clamp 2..=64，默认 8）：超限优先 LRU 淘汰空闲项，全忙时关停最旧项；当前会话不计入。淘汰后切回走 `build_runtime` 从存储重建。
- `/delete` 软删整个子树（含后代）并按返回 id 关停全部对应 runtime、拒绝其审批、清理跟踪表；删除最后一个会话时新建替代会话。
- `AppService::start` 先 `WorkspaceLock::acquire` 独占 canonical workspace，第二个程序打开同一 workspace 立即失败；drop 最后一个 `AppHandle` 才释放锁并收尾 engine 任务。
- 消费端不拥有 runtime/审批 oneshot：adapter 断开重连必须先取 snapshot，再按 `event_cursor` replay 后 subscribe，游标逐出即 resync。
- undo/redo 移动 head 后按 `file_snapshots` 回滚/前滚目标文件（undo 写 pre_image、redo 写 post_image；无快照的路径跳过不误伤）。快照上限：单文件 `checkpoint_max_file_bytes`（clamp 4KB..=8MB，超限记 marker 提示"未回滚"）、单会话 `checkpoint_max_session_bytes`（clamp 1MB..=256MB，超限丢最旧）；回滚 IO 失败不阻断会话恢复，只在 status 提示。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 删除后任务仍跑 | `deleted_ids` 覆盖 -> `shutdown` 调用 -> abort 顺序 |
| 删除后审批悬空 | oneshot 拒绝先于 abort -> owner 路由 |
| 后台内存增长 | 容量配置 -> 淘汰触发点（切换/终态事件） -> 全忙关停 |
| 切回会话丢流式状态 | 是否被淘汰 -> `build_runtime` 重建路径 |
| 面板残留子会话 | `refresh_sessions` 收敛 -> `child_batches`/`child_status` 清理 |
| adapter 断连/重连丢事件 | 重连先 snapshot -> `event_cursor` replay -> subscribe 时序 -> 重复订阅 |
| 多消费端并发命令错乱 | 是否绕过 `AppHandle` -> `CoreCommand` 串行 -> 直改核心 |

## 验证

- 迭代过滤器：`delete_`、`background_capacity`、`switching_session`、`handle_routed_event`、`workspace_lock`。
- 生命周期、容量、取消或工作区锁协议变更升级到完整测试和 Clippy。
- 新增 `AgentEvent` 变体需一次接通：agent 内 forward 闭包（`Forwarded::Send`/`SendIgnore`/`Ignore` 语义按需选）→ `session.rs handle_event` 穷尽 match → `app.rs` 路由/`should_coalesce_stream_redraw` 是否合并；压缩与子 agent 的 `|_| Ignore` 闭包自动忽略但行为要确认；跨层接线链（ModelEvent→AgentEvent→Event→消费端）见 UI Contract 专题。
- app 层测试注意：`test_app` 构造的 `SessionRuntime` 含 tokio 组件，涉及审批/undo 的测试必须 `#[tokio::test]`；改 `resolve_approval` 等被测试直接调用的签名会连带旧调用点编译失败，改动时一并扫 `grep resolve_approval(`。
