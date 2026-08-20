# 工具维护指南

## 适用范围

内置工具、参数 schema、执行分发、权限分类、子 Agent 工具过滤、Prompt 清单、UI 翻译，以及 `Workspace` 路径边界。

## 入口

- `src/tools/mod.rs`：`definitions()`、`execute()`、`policy()` 与只读模式拦截。
- `src/tools/filesystem.rs`、`git.rs`、`process.rs`、`web.rs`：各领域实现。
- `src/security.rs`：`Workspace` 解析、`classify_tool`。
- 下游接入：`src/agent.rs`（`WRITE_TOOLS`/角色 infer）、`src/prompt.rs`、`src/ui.rs`（翻译/风险/摘要）。

## 不变量

- 新增内置工具必须一次接通全部接入点，缺一会静默漏权限或漏提示：
  `definitions()` schema（`deny_unknown_fields` + `additionalProperties:false`）→ `execute()` 分发 → `policy()` 只读 Deny 名单 → `classify_tool`（mutating 归 RequireApproval）→ agent 的 `WRITE_TOOLS`/角色 infer → prompt 三处清单 → ui 的 `tool_display_name`/`tool_compact_summary`/`tool_risk`/`argument_label` → ui 翻译测试清单。
- `Workspace::resolve_*` 是唯一合法路径解析点：拒绝 `..`、绝对路径逃逸与符号链接逃逸；新目标验证 canonical parent；不要在工具实现里自行拼路径。
- 参数对象一律 `#[serde(deny_unknown_fields)]`，未知键直接报 `InvalidArguments`，不给模型静默传错的空间。
- 危险操作保持"默认 Deny、mutating 需审批、Plan/Explore 只读拦截、`permissions.tools` 可覆盖"四层语义；`classify_tool` 返回 `Deny(unknown tool)`，未知工具不落入 Allow。
- 外部进程工具必须带超时、输出截断、取消与进程树清理；`web` 每次重定向校验 HTTP/HTTPS 与公网地址。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 工具不显示/模型说不可用 | `definitions()` 注册 -> `execute()` 分发 -> prompt 清单 |
| 只读模式下工具仍可用 | `policy()` 只读名单 -> `classify_tool` -> `permissions.tools` 覆盖 |
| 审批弹窗缺/多余 | `classify_tool` 分类 -> `policy()` override -> git 特例 |
| 子 Agent 拿不到工具 | `WRITE_TOOLS`/角色 infer -> `child_tool_name_allowed` -> allowed_tools 模板 |
| 路径逃逸或误改工作区外文件 | 参数路径来源 -> `Workspace::resolve_*` -> canonical parent |

## 验证

- 迭代过滤器按改动选择：`tools::filesystem::tests`、`security::tests`、`tools::process::tests`；UI 翻译改动加 `ui::tests`。
- 新增工具至少覆盖：成功路径 + 读回验证、失败/拒绝路径、路径逃逸拒绝；mutating 工具加 `security::tests` 审批断言。
- 完成阶段按根文档运行一次 lib 测试；工具/存储/安全/进程跨模块升级到完整测试和 Clippy。
