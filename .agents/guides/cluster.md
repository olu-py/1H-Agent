# 集群与审批维护指南

## 适用范围

`agent_spawn`、子 Agent 调度、执行预算、进度状态、审批、取消和父子会话结果。

## 入口

- `src/agent.rs`：`AgentRunner`、`child_slots`、`ChildSessionStatus`、`ChildSessionProgress`、子循环与结果 JSON。
- `src/config.rs`：`ClusterConfig` 和 Agent 模板；`src/app.rs`/`session.rs`：批次、审批 owner 与状态路由。
- `src/prompt.rs`/`tools/mod.rs`：集群契约、spawn schema 和子工具过滤。

## 不变量

- 子 Agent 仅一层、不能 spawn、无终端；写入仍经过 mode、安全和审批。
- 同一父 runner 的 clone 共享 `child_slots`，默认并发 4；不同父 runner 不共享 semaphore，App runtime 共享审批锁。
- 主动预算默认 300 秒，只计模型和工具；并发槽、审批槽和用户等待不计。范围由 `Config::load` 归一化。
- `max_turns = 0` 无固定轮次；正数才产生 `turn_limit`，预算和资源上限始终有效。
- 同轮 spawn 结果齐全后主 Agent 才继续；单个完成时立即持久化和更新，不等整批。
- 机器终态固定为 `completed`、`failed`、`turn_limit`、`timed_out`、`cancelled`；失败也携带部分 output。
- 进度只传阶段、轮次、工具和时间；排队、模型、流式、工具、审批槽、用户审批和终态可区分。
- 全局展示最早审批并按 owner 路由；取消覆盖全部等待/执行阶段，释放 permit/lock 并可靠发送终态。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 批次停滞 | 阶段/批次计数 -> 首帧 -> 工具 -> 审批槽/用户审批 |
| 完成项不显示 | 单项更新时机 -> RoutedEvent 父子 session 路由 |
| 审批不可见/Y-N 无效 | 全局最早项 -> owner -> oneshot -> 锁范围 |
| 取消后仍运行 | token -> select -> future/drop guard -> 终态 -> permit |
| 主 Agent 无结果 | 每个 tool call ID 是否在失败/超时路径也返回结构化 output |

## 验证

```bash
cargo test child_status_has_stable_wire_names_and_localized_labels
cargo test child_concurrency_slots_enforce_the_configured_limit
cargo test cancellation_progress_survives_a_temporarily_full_channel
cargo test background_child_approval_is_globally_visible_and_routed
cargo test cluster_batch_status_tracks_queued_running_and_completed_children
cargo test main_agent_completes_after_one_hundred_tool_rounds
```
