# Provider 维护指南

## 适用范围

Provider 档案、设置、密钥、请求协议、reasoning、`response_id`、上下文压缩和会话恢复。

## 入口

- 配置：`ProviderConfig`、`ProviderPreset`、`provider_for`、`upsert_provider`、`remove_provider`。
- 密钥/设置：`api_key_cached*`、`store_api_key_cached`、连接列表与模板表单。
- 请求/恢复：`replay_safe_items`、请求游标、`provider/openai.rs`、`storage.rs` 的 Provider 状态。

## 不变量

- `Config.provider` 是当前连接；`Config.providers` 按预设唯一保存完整档案。旧 `[provider]` 无损迁移，API Key 永不序列化。
- 非密钥配置按默认值 -> TOML -> 环境变量覆盖；模板只用 `ProviderPreset::defaults`，不得复制默认 URL。
- 启动用 `api_key_cached` 预热；运行期只用 `api_key_cached_only`；新密钥通过 `store_api_key_cached` 同步钥匙串和内存。
- 切换 Provider/模型必须重建 runner 并清理旧 `response_id`。增量游标从最新用户消息开始且保留其后 `@` 上下文。
- 压缩检查点和 `/uncompact` 都清理 `previous_response_id`；压缩摘要不得与旧服务端状态混用。
- 服务端状态失效后先清 ID，再用 `replay_safe_items` 重放；不得发送孤立 output 或无结果 call。
- DeepSeek Responses 不用 previous ID；原生搜索与同名本地 tool 互斥。
- Reasoning 事件按增量语义处理：空 content 不结束思考，done 的完整文本不重复追加；Qwen 3.7/3.8 字段按各协议隔离。
- 私有 JSON/SSE 必须先规范化为公共事件；诊断输出始终脱敏。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| orphan tool output 400 | 游标 -> call/output ID -> response ID -> replay 过滤 |
| 切换后配置回退 | profile -> active 副本 -> session provider/model -> runner rebuild |
| 重复钥匙串弹窗 | 热路径 key 查询 -> cache-only -> 缓存错误是否被错误重试 |
| 请求/SSE 400 | `ProviderKind` -> body/tool/thinking 字段 -> SSE 终态 |

## 验证

```bash
cargo test config::tests
cargo test settings::tests
cargo test secrets::tests
cargo test provider::openai::tests
cargo test incremental_cursor_keeps_latest_user_message_and_following_context
cargo test stateless_replay_keeps_only_complete_ordered_tool_pairs
```
