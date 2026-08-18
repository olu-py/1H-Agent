# 集群与审批维护指南

仅在修改 `agent_spawn`、子 Agent、审批、取消、进度状态或集群会话树时读取本文件。

## 先看哪些符号

- `src/agent.rs`：`AgentRunner`、`child_slots`、`ChildSessionStatus`、`ChildSessionProgress`、子 Agent 主循环和结果 JSON。
- `src/config.rs`：`ClusterConfig`、Agent 模板及默认值和范围校验。
- `src/app.rs` / `src/session.rs`：批次统计、全局审批查找、owner session 路由和状态持久化。
- `src/prompt.rs` / `src/tools/mod.rs`：集群提示契约、`agent_spawn` schema 和子工具过滤。

## 必须保持的不变量

- 子 Agent 只有一层，不能再次 spawn，也没有终端工具；写入仍经过 mode、安全策略和审批。
- 同一父 `AgentRunner` 的 clone 共享 `child_slots`；默认并发为 4。不同父 runner 不共享该 semaphore，同一 App 的 runtime 共享全局审批锁。
- 默认主动执行预算为 300 秒，只累计模型请求和工具执行；等待并发槽、审批槽和用户审批不计时。具体范围以 `Config::load` 中的归一化和相邻测试为准。
- `max_turns = 0` 表示无固定轮次上限；显式正数才产生 `turn_limit`，主动预算与资源上限仍始终有效。
- 同一模型轮次的 `agent_spawn` 工具结果必须全部返回后主 Agent 才能继续；单个子 Agent 完成时立即持久化和更新工具行，不等整批结束。
- 机器终态固定为 `completed`、`failed`、`turn_limit`、`timed_out`、`cancelled`；中文只来自 `label()`。非完成结果也保留部分 output。
- 进度事件只报告阶段、轮次、工具和更新时间，不转发全文 delta；排队、模型、流式、工具、审批槽、用户审批及所有终态必须可区分。
- 审批全局展示最早待处理项，并按 owner session 路由 Y/N；未取得锁是“等待审批槽”，取得后才是“等待用户审批”。
- 父任务取消覆盖排队、stream、工具、审批槽和审批等待，释放 permit/lock 并可靠发送终态；bounded channel 暂满不能留下永久“运行中”。

## 常见故障顺序

- 批次似乎停住：先看子状态和批次计数，区分排队、模型无首帧、工具、审批槽和用户审批；不要把所有等待归因于模型。
- 完成项迟迟不显示：检查是否错误等待整个 join 集合后才更新，以及 RoutedEvent 是否按父/子 session 正确路由。
- 审批不可见或 Y/N 无效：检查全局最早项、owner session、oneshot sender 和锁持有范围。
- 取消后仍运行：沿 cancellation token -> select 分支 -> child future/drop guard -> terminal progress -> permit 释放逐项确认。
- 主 Agent 收不到结果：检查每个 tool call ID 是否得到一个结构化输出，失败和超时路径也不能漏发。

## 最小验证命令

```bash
cargo test child_status_has_stable_wire_names_and_localized_labels
cargo test child_concurrency_slots_enforce_the_configured_limit
cargo test cancellation_progress_survives_a_temporarily_full_channel
cargo test background_child_approval_is_globally_visible_and_routed
cargo test cluster_batch_status_tracks_queued_running_and_completed_children
cargo test main_agent_completes_after_one_hundred_tool_rounds
```

改动调度、取消或审批协议时，必须再运行完整 Clippy 与测试。
