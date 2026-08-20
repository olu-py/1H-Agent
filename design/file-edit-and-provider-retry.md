# 任务：file_edit 结构化编辑工具 + Provider 重试退避

> 来源：2026-08-20 计划评审。两项独立改动，均为纯新增行为，不改变现有语义。
> 每完成一个任务勾选对应复选框；全部完成后按文末验证矩阵收尾。

---

## 任务 A：`file_edit` 结构化编辑工具（str_replace 式）

背景：目前模型编辑文件只有整文件覆写的 `file_write`（`src/tools/filesystem.rs:176`），
改一行也要重传全文，token 消耗和出错率都差一个量级。新增 `file_edit`：
`old_string`/`new_string` 精确替换，默认要求唯一匹配。

### A1. 核心实现 `src/tools/filesystem.rs`

- [x] 新增 `EditArgs`（沿用 `deny_unknown_fields` 模式）：

  ```rust
  #[derive(Deserialize)]
  #[serde(deny_unknown_fields)]
  struct EditArgs {
      path: String,
      old_string: String,
      new_string: String,
      replace_all: Option<bool>,
  }
  ```

- [x] 新增 `pub fn edit(workspace: &Workspace, value: &Value) -> Result<String, ToolError>`：
  - `workspace.resolve_existing(path)`（文件必须已存在，与 `file_read` 一致；复用 `security_error` 转换）
  - `fs::read_to_string` 读入；非 UTF-8 报 `ToolError::Execution("file is not valid UTF-8")`
  - `old_string` 为空 -> Execution 错误
  - 统计 `old_string` 出现次数：
    - 0 次 -> 错误提示"未找到匹配，请先 file_read 确认内容"
    - 多于 1 次且 `replace_all != Some(true)` -> 错误提示扩大 old_string 上下文或改用 replace_all
    - 1 次或 replace_all=true -> 替换并写回
  - 返回格式仿照 `write`：`"edited {relative_path}: {n} replacement(s)"`（`display_relative`）

### A2. 接入点（每处一行）

- [x] `src/tools/mod.rs:160` `definitions()`：新增 `file_edit` 定义；
      schema：`path`/`old_string`/`new_string` 必填、`replace_all` 可选 bool、
      `additionalProperties:false`；描述写明唯一匹配语义
- [x] `src/tools/mod.rs:326` `execute()`：`"file_edit" => filesystem::edit(&self.workspace, &call.arguments)`
- [x] `src/tools/mod.rs:66-77` 只读模式拦截：`"file_edit"` 加入 Plan 模式 Deny 名单
- [x] `src/security.rs:102` `classify_tool`：`"file_edit"` 加入 RequireApproval 分支
      （理由文案 `file_edit changes workspace files`）
- [x] `src/agent.rs:86` `WRITE_TOOLS`：加 `"file_edit"`
- [x] `src/agent.rs:1671-1674` 角色 infer（allowed_tools 判断）：加 `tool == "file_edit"`
- [x] `src/prompt.rs:65` 工具清单：加 `file_edit`
- [x] `src/prompt.rs:30` 与 `:101` cluster 提示词：implement 角色可写工具清单加 `file_edit`
- [x] `src/ui.rs:1608` `tool_display_name`：`"file_edit" => Some("文件编辑")`
- [x] `src/ui.rs:1643` `tool_compact_summary`：`"file_edit"` 取 path
- [x] `src/ui.rs:2204` `tool_risk`：`"file_edit"` -> MEDIUM - changes workspace files
- [x] `src/ui.rs:2228` `argument_label`：`"old_string" => "原文本"`、`"new_string" => "新文本"`
- [x] `src/ui.rs:3399` 测试清单 `all_builtin_tool_names_are_translated_...`：加 `("file_edit", "文件编辑")`

### A3. 测试

- [x] `src/tools/filesystem.rs` tests（tempfile 模式）：
  - 替换成功并 `read` 验证内容
  - 0 匹配报错
  - 多匹配未开 replace_all 报错
  - replace_all=true 全部替换
  - 路径逃逸拒绝（`../`）
- [x] `src/security.rs` tests：`file_edit` 需审批断言（仿 `mutations_require_approval`）

---

## 任务 B：Provider 重试与指数退避

背景：`src/provider/openai.rs` 无任何 retry/backoff/429 处理；网络抖动或限流直接
报错终止一轮。目标：HTTP 层对"尚未发出任何事件"的失败做指数退避重试，
流中断不重试（避免 delta 重放，由 agent.rs:1216 现有空输出重放兜底）。

### B1. 配置层 `src/config.rs`

- [x] `ProviderConfig`（:73）新增三字段（struct 已有 `#[serde(default)]`，旧配置自动兼容）：

  ```rust
  pub retry_max_attempts: u32,        // 默认 3（0 = 关闭）
  pub retry_initial_backoff_ms: u64,  // 默认 500
  pub retry_max_backoff_ms: u64,      // 默认 8000
  ```

- [x] `Config::load` clamp 区（:533 附近）：`retry_max_attempts` clamp 0..=5、
      `retry_initial_backoff_ms` clamp 100..=2000、`retry_max_backoff_ms` clamp 1000..=30000
- [x] `ProviderPreset::defaults()`（:759）填默认值
- [x] `config/config.example.toml` `[provider]` 块：三个键 + 注释说明（含 0 = 关闭）
- [x] `config.rs:1247` `defaults_are_bounded`：加重试键断言
- [x] 新增 clamp 边界测试（仿 `background_session_limit_is_normalized`，:1259）

### B2. 错误类型与重试决策 `src/provider/mod.rs` + `openai.rs`

- [x] `ProviderError::Status`（provider/mod.rs:112）增加 `retry_after_ms: Option<u64>` 字段
      （构造点仅 openai.rs:87 一处；agent.rs:741 压平成 String，不受影响）
- [x] `openai.rs` 状态码错误路径（:82-91）解析 `Retry-After` 头（秒数或 HTTP-date 一律
      折算为毫秒；解析失败按 None），填入 `retry_after_ms`
- [x] 新增纯函数（可单测）：

  ```rust
  pub(crate) fn retry_delay(
      error: &ProviderError, attempt: u32,
      initial_ms: u64, max_ms: u64,
  ) -> Option<Duration>   // None = 不可重试
  ```

  判定规则：
  - `Http(e)`：仅 `e.is_connect() || e.is_request()` 可重试（发送阶段失败）
  - `Status { status: 408|429|500|502|503|504, .. }`：可重试；`retry_after_ms` 存在时优先
    （clamp 到 max_backoff），否则指数退避 `initial_ms * 2^(attempt-1)`，上限 max_backoff
  - 其他 `Status`、`Protocol`、`ReceiverClosed`：不可重试（ReceiverClosed 是取消路径，绝不重试）

### B3. `stream()` 重构 `src/provider/openai.rs:49`

- [x] 现有逻辑抽为 `stream_attempt()`；`stream()` 外层循环：

  ```text
  attempt = 1
  loop:
      match stream_attempt():
          Ok -> return Ok
          Err(e) if attempt < retry_max_attempts 且 未发出过任何事件:
              delay = retry_delay(e, attempt, ...) else return Err(e)
              发送 ModelEvent::Retrying（见 B4）
              tracing::warn!(status, attempt)   // 脱敏：不记 body
              tokio::time::sleep(delay).await   // abort 安全
              attempt += 1
          Err(e) -> return Err(e)
  ```

- [x] 关键约束：
  - `emitted` 标志：`stream_attempt` 一旦向 events channel 发出过任何事件，
    后续失败一律不重试（避免 delta 重放导致 UI 文本重复）
  - 流中途断开（chunk 错误）不重试
  - sleep 期间被 Esc abort 时任务直接结束（JoinHandle::abort 机制，天然取消安全）
  - reqwest 10min timeout 每请求生效，重试总时长 = 尝试×超时+退避，可接受
  - `#[cfg(test)]` scripted 分支保持不变
- [x] `OpenAiClient` 新增 `retry` 配置字段；`new` 签名扩展（或 `with_retry` builder）；
      三个调用点统一从 `ProviderConfig` 取：`agent.rs:870`、`app.rs:2344`、`app.rs:2905`

### B4. 重试可见性：事件贯穿

- [x] `ModelEvent`（provider/mod.rs:72）加 `Retrying { attempt: u32, reason: String, delay_ms: u64 }`
      （`StreamCollector::on_event` 的 `other => Some(other)` 自动透传）
- [x] `AgentEvent`（agent.rs:473）加 `ProviderRetry { attempt, reason, delay_ms }`
- [x] 主循环 forward 闭包（agent.rs:1133）加分支：
      `Ok(Forwarded::SendIgnore(AgentEvent::ProviderRetry { .. }))`
- [x] `session.rs handle_event`（:149）加分支：
      `self.status = format!("请求失败，{delay} 秒后第 {attempt} 次重试（{reason}）")`，
      不改变 phase
- [x] 压缩路径（agent.rs:1008）与子 agent 路径（agent.rs:1824）的 `|_| Ok(Forwarded::Ignore)`
      闭包自动忽略，无需改动
- [x] `should_coalesce_stream_redraw`（app.rs:2639）只匹配 TextDelta/ReasoningDelta，
      重试事件低频，无需加

### B5. 测试

- [x] `provider::openai::tests`：`retry_delay` 纯函数单测（各状态码、Retry-After 优先、
      指数序列、上限、不可重试类别、Http 分类）
- [x] 流式重试集成测：扩展 scripted 机制，新增 `scripted_failures` 钩子
      （`VecDeque<ProviderError>` 先弹出注入失败再走 scripted_events），
      验证"先 429 -> 重试 -> 成功"且事件不重复
- [x] `config::tests`：clamp 边界（B1）
- [x] agent 层：scripted 失败场景验证 `ProviderRetry` 事件透传到 UI 事件

---

## 文档同步

- [x] `.agents/guides/provider.md`：补重试契约与可重试错误分类（遵守 50 行上限与
      适用范围/入口/不变量/诊断/验证 六节结构）
- [x] 检查 `AGENTS.md` 是否有需要同步的事实（一个事实只归属根文档或一个专题）

---

## 验证矩阵（按 AGENTS.md）

| 阶段 | 命令 |
| --- | --- |
| 迭代中（各跑一次相关过滤器） | `cargo test --lib --all-features --locked -- filesystem`；`-- openai::tests`；`-- config::tests`；`-- security` |
| 局部完成 | `cargo fmt --all -- --check`；`cargo test --lib --all-features --locked` |
| 跨模块收尾（工具+provider） | `cargo clippy --all-targets --all-features --locked -- -D warnings`；`cargo test --all-features --locked` |
| 文档 | `bash scripts/check-agent-docs.sh`；`git diff --check` |

## 已定决策（未另行确认时按此执行）

- 重试仅在 HTTP 层（事件未发出前）；流中断不重试--不与 agent 层 response_id 重放冲突
- 重试参数走配置键 + clamp--符合"所有容量有硬上限"不变量
- 重试时状态栏提示--新增 `AgentEvent::ProviderRetry`
- `file_edit` 唯一匹配 + `replace_all` 可选--与主流 agent 一致，最防误改
