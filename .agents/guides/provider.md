# Provider 维护指南

## 适用范围

Provider 档案、设置、密钥、请求协议、reasoning、`response_id`、上下文压缩和会话恢复。

## 入口

- 配置：`ProviderConfig`、`ProviderPreset`、`provider_for`、`upsert_provider`、`remove_provider`。
- 首页选择：`HomeSelection`、`apply_home_selection`。
- 密钥/设置：`api_key_cached*`、`store_api_key_cached`、连接列表与模板表单。
- 请求/恢复：`replay_safe_items`、请求游标、`provider/openai.rs`、`storage.rs` 的 Provider 状态。

## 不变量

- `Config.provider` 是当前连接；`Config.providers` 按预设唯一保存完整档案。旧 `[provider]` 无损迁移，API Key 永不序列化。
- 非密钥配置按默认值 -> TOML -> 环境变量覆盖；模板只用 `ProviderPreset::defaults`，不得复制默认 URL。
- 启动只用 `api_key_cached` 解锁当前 Provider 一次，其他环境变量密钥可无交互预热；不得遍历独立钥匙串条目。显式切换/编辑 Provider 可按需解锁一次，Agent 热路径只用 `api_key_cached_only`；新密钥通过 `store_api_key_cached` 同步钥匙串和内存。
- 首页只复制按 preset 去重的非密钥档案；仅 `StartNew` 将所选 Provider/模型/mode 应用到配置与新会话并按需解锁，`Resume` 仍恢复目标会话状态。
- 切换 Provider/模型必须重建 runner 并清理旧 `response_id`。增量游标从最新用户消息开始且保留其后 `@` 上下文。
- 压缩检查点和 `/uncompact` 都清理 `previous_response_id`；压缩摘要不得与旧服务端状态混用。
- 服务端状态失效后先清 ID，再用 `replay_safe_items` 重放；不得发送孤立 output 或无结果 call。
- DeepSeek Responses 不用 previous ID；原生搜索与同名本地 tool 互斥。
- Reasoning 事件按增量语义处理：空 content 不结束思考，done 的完整文本不重复追加；Qwen 3.7/3.8 字段按各协议隔离。
- 私有 JSON/SSE 必须先规范化为公共事件；诊断输出始终脱敏。
- HTTP 层指数退避重试仅在"未发出任何事件"的失败上生效（连接/发送阶段错误与 408/429/500/502/503/504）；流中断不重试，由 agent 层空输出重放兜底；`Retry-After` 优先并被 clamp 到 `retry_max_backoff_ms`。重试上限与退避参数来自 `ProviderConfig`（0 关闭）并 clamp。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| orphan tool output 400 | 游标 -> call/output ID -> response ID -> replay 过滤 |
| 切换后配置回退 | profile -> active 副本 -> session provider/model -> runner rebuild |
| 重复钥匙串弹窗 | 热路径 key 查询 -> cache-only -> 缓存错误是否被错误重试 |
| 请求/SSE 400 | `ProviderKind` -> body/tool/thinking 字段 -> SSE 终态 |
| 请求失败但无重试 | `retry_max_attempts`/clamp -> 错误分类（`retry_delay`）-> 是否已发事件 |

## 验证

- 迭代过滤器：`config::tests`、`settings::tests`、`secrets::tests`、`provider::openai::tests`、`provider::tests`（重试决策）。
- Agent 状态过滤器：`incremental_cursor_keeps_latest_user_message_and_following_context`、`stateless_replay_keeps_only_complete_ordered_tool_pairs`、`provider_retry_event_reaches_the_ui_channel`。
- 完成阶段按根文档运行一次 lib 测试；涉及存储恢复时升级到完整测试。
- 重试测试用 `OpenAiClient::scripted_with_failures`/`scripted_steps`（`Fail`/`Events`/`EventsThenFail`）模拟"发出事件后再失败"的流中断，验证不重试防 delta 重放；集成测用 1ms 退避避免 flaky。
