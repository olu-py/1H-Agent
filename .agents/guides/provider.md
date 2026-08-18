# Provider 维护指南

仅在修改 Provider、模型、设置、密钥、会话恢复或请求协议时读取本文件。

## 先看哪些符号

- `src/config.rs`：`ProviderConfig`、`ProviderPreset`、`Config::provider_for`、`upsert_provider`、`remove_provider`。
- `src/settings.rs`：连接列表、模板排重、表单校验和 API Key 复用。
- `src/secrets.rs`：`api_key_cached`、`api_key_cached_only`、`store_api_key_cached`。
- `src/agent.rs`：请求游标、`replay_safe_items`、runner 重建和跨 Provider 子 Agent。
- `src/provider/openai.rs` 与 `src/storage.rs`：协议/SSE、`response_id` 持久化。

## 必须保持的不变量

- `Config.provider` 是当前激活连接；`Config.providers` 按 `ProviderPreset` 唯一保存完整档案。旧单一 `[provider]` 必须无损迁移，API Key 永不序列化。
- 非密钥配置遵循内置默认值 -> TOML -> 环境变量覆盖。模板默认值由 `ProviderPreset::defaults` 统一提供，不能在 UI 或 Agent 中复制 URL。
- 启动阶段用 `api_key_cached` 预热已连接预设；运行期切换、设置、恢复和子 Agent 只能用 `api_key_cached_only`，避免再次触发钥匙串密码。新密钥用 `store_api_key_cached` 同步钥匙串和内存缓存。
- 切换 Provider 或模型必须重建 runner，并清理当前会话旧 `response_id`，不能跨端点复用服务端状态。
- 使用 `previous_response_id` 时，请求游标从最新用户消息开始并保留其后的 `@` 上下文；不能使用简单的末项切片。
- 上下文压缩后，`CompactionSummary` 与保留的最近完整轮次成为新的本地 canonical context；提交压缩检查点和 `/uncompact` 恢复都必须清理 `previous_response_id`，不得把压缩摘要与旧服务端状态混用。
- 服务端状态失效后先清理持久化 ID，再通过 `replay_safe_items` 重放；孤立 `ToolOutput` 和没有结果的残缺 call 不得进入请求。
- DeepSeek Responses 保持无 `previous_response_id`；启用原生搜索时发送 Provider `web_search`，并排除同名本地 function tool。
- Qwen Chat 的 `reasoning_content` 与 Responses 的 `response.reasoning_text.delta` 都是增量文本；空 `content` 不得结束思考，`response.reasoning_text.done` 的完整文本不得再次追加。Qwen 3.7 Chat 使用 `enable_thinking`/`thinking_budget`，Qwen 3.8 Chat 与 Qwen Responses 使用各自文档规定的 reasoning effort 结构。
- Provider 私有 JSON 和 SSE 必须先转换为公共模型事件；UI、存储和工具层不解析私有协议。

## 常见故障顺序

- `No tool call found for tool output`：检查增量游标 -> call/output ID 配对 -> `response_id` 生命周期 -> 重放过滤；不要先增加重试。
- 切换后配置回退：检查 `provider_for`/`upsert_provider` -> active 副本 -> session provider/model -> runner rebuild；禁止回退到预设 URL 掩盖档案丢失。
- 重复钥匙串弹窗：搜索运行热路径中的 `api_key`/`api_key_cached`，应改走已预热的 cache-only 查询；不得吞掉缓存错误后再次访问系统钥匙串。
- 400 或流解析失败：核对 `ProviderKind`、请求 body、工具定义、thinking 字段和 SSE 终态；诊断输出必须脱敏。

## 最小验证命令

```bash
cargo test config::tests
cargo test settings::tests
cargo test secrets::tests
cargo test provider::openai::tests
cargo test incremental_cursor_keeps_latest_user_message_and_following_context
cargo test stateless_replay_keeps_only_complete_ordered_tool_pairs
```

同时影响配置、Agent 和存储契约时，再运行根文档规定的完整 Clippy 与测试。
