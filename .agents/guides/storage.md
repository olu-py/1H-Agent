# 存储维护指南

## 适用范围

`Storage` 的 schema 与迁移、会话树、消息/tool_calls/provider_state 持久化、软删与子树、undo/redo head 移动、`file_snapshots` 快照生命周期——全部由 core 独占。

## 入口

- `crates/protium-core/src/storage.rs`：`from_connection`（建表 + `ensure_column` 兼容旧库 + `backfill_turns`）、全部读写方法。
- `crates/protium-core/src/service.rs`/`session.rs`：undo/redo/delete/fork 命令与文件回滚交互。
- 消费端只经 `AppHandle` 提交命令；`snapshot`/`messages` 是读取历史的唯一入口。

## 不变量

- 单连接 + `Arc<Mutex<Connection>>`，WAL + foreign_keys ON。表全部外键 `ON DELETE CASCADE`，但删除会话走软删（`deleted_at`），CASCADE 实际永不触发。
- SQLite/WAL、会话树、消息、provider state 和 file snapshots 全部由 core 独占；消费端不得直接打开数据库或绕过 `AppHandle` 查询/写入。
- `snapshot`/`messages` 是消费端读取历史的唯一入口；恢复沿 `head_turn_id` 父链，隐藏的 compaction 消息被过滤。
- 新表走 `CREATE TABLE IF NOT EXISTS` + 迁移版本号；旧库缺列用 `ensure_column`（PRAGMA table_info 探测）补，不用 ALTER IF NOT EXISTS。改表语义必须覆盖"已存在旧库"路径并加迁移版本。
- 消息一律挂在当前 `head_turn_id` 对应的 turn 上（`append_typed_item` 模式）；`load_messages` 沿 head 父链递归取活链。
- undo/redo 只把 `head_turn_id` 移到 parent/child；会话内容回滚由 app 层按 `file_snapshots` 完成，storage 只提供 `restore_turn_files`/`turns_between` 查询。
- 快照写工具调用前由 agent 层捕获 pre_image、执行后回填 post_image；单文件超 `checkpoint_max_file_bytes` 存 marker（existed=0），单会话超 `checkpoint_max_session_bytes` 丢最旧，均由 storage 在写事务内 enforce。
- 软删子树返回全部后代 id 供调用方关停 runtime；快照随软删由 `purge_soft_deleted_snapshots` 清理（`delete_session` 路径）。
- 审计链：`tool_calls.decision` 记 `allowed`/`approved`/`rejected`/`denied`/`session-allowed`，新增策略来源需同步这里。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 旧库打开报 missing column | 建表语句 -> `ensure_column` 列表 -> 迁移版本 |
| undo 后对话对但文件没回滚 | head 移动 -> `file_snapshots` 是否有该 turn -> 单文件/单会话上限 marker |
| 删除后快照残留 | 软删路径 -> `purge_soft_deleted_snapshots` 调用点 |
| 消息挂错 turn | `append_*` 的 head 查询 -> turn 创建/移动时机 |
| fork 丢内容 | 消息/任务复制循环 -> head_turn 初始化 |
| 消费端绕过 AppHandle 读库 | 是否直开 DB -> 是否只经 `snapshot`/`messages` -> 并发写命令串行 |

## 验证

- 迭代过滤器：`storage::tests`（含 `file_snapshots_*`、`turns_between`、`purge_soft_deleted`）。
- undo/redo 行为经 app 层验证：`undo_rolls_back_snapshotted_file_and_redo_restores_it`、`undo_without_snapshot_keeps_file_untouched`。
- 涉及 schema/迁移改动时升级到完整测试（覆盖内存库与落盘库 `Storage::open` 两条路径）与 Clippy。
