# 1H-Agent AI 维护协议

> 读取对象：维护和开发本仓库的 AI Agent。先读本文件，再按任务路由读取目标源码；不要为了背景一次读取整个仓库。

## 0. 稳定上下文

```text
project: 1H-Agent
meaning: 1H = 氕（protium），不是“一小时”
goal: 极致轻量化、高性能、权限感知的跨平台终端 Agent
runtime: 单个 Rust/Tokio 进程，Ratatui + Crossterm，SQLite/WAL
authority: 源码 > config/config.example.toml > .github/workflows > 本文件
scope: TUI、模型流、受控工具、会话与跨平台发布
excluded: Web UI、内置浏览器、远程 MCP、动态插件、图片/语音能力
cache_rule: 稳定规则置前；任务事实按源码路由按需读取
```

不可违反：保持单 Rust 二进制；不引入 Node.js、Python、Chromium、Web UI、动态插件 ABI 或后台轮询。所有路径、网络、工具、外部进程和内存增长均须有本地边界与上限。

## 1. 任务路由

| 任务 | 首读文件 | 关联文件 |
| --- | --- | --- |
| TUI、快捷键、渲染、会话切换 | `src/app.rs` | `src/input.rs`、`src/ui.rs`、`src/commands.rs` |
| 系统提示词、Agent loop、审批、子 Agent | `src/agent.rs` | `src/prompt.rs`、`src/provider/mod.rs` |
| Chat/Responses、SSE、DeepSeek 原生搜索 | `src/provider/openai.rs` | `src/provider/mod.rs`、`src/config.rs` |
| 文件、命令、Git、Web、browser/MCP | `src/tools/mod.rs` | `src/tools/*.rs`、`src/security.rs` |
| 路径边界、SSRF、工具默认策略 | `src/security.rs` | `src/tools/process.rs`、`src/tools/web.rs` |
| TOML、环境变量、Provider、资源默认值 | `src/config.rs` | `config/config.example.toml`、`src/secrets.rs` |
| 会话、分支、迁移、持久化 | `src/storage.rs` | `src/provider/mod.rs` |
| CI、release、安装包 | `.github/workflows/ci.yml` | `.github/workflows/release.yml`、`Cargo.toml` |

任务只读取涉及的行和直接依赖。发现本文件与事实来源不一致时，以事实来源为准，并在同一改动中更新本文件。

## 2. 架构与数据流

```text
terminal event -> app::App -> agent::AgentRunner -> provider::OpenAiClient
                     |              |                    |
                     |              +-> ToolRegistry <--- ToolCall
                     +-> Storage(SQLite/WAL) <- events/results
                                    |
                               secrets(OS keyring)
```

- `app` 只维护 UI 状态、调度与持久化入口；`ui` 只渲染快照；`input` 只编辑 UTF-8 文本。
- `agent` 负责有界多 turn 循环、模型事件、审批和工具结果；`provider` 负责协议/SSE 转换；`tools` 负责分发。
- 跨模块使用 `ConversationItem`、`ModelRequest`、`ModelEvent`、`ToolCall`、`ToolDefinition`、`Usage`，不要把 Provider 私有 JSON 泄漏到 UI。
- 正常路径：输入 -> 可选 `@` context -> `ConversationItem` -> 流式 `ModelEvent` -> 文本/工具事件按原顺序显示与持久化 -> 下一 turn。
- `Esc` 必须取消活跃模型、等待审批和外部进程；恢复会话时按 `head_turn_id` 的父链读取可见消息。

## 3. 安全与权限

- workspace 在启动时 canonicalize。已有目标必须在根目录内；新目标必须验证 canonical parent；拒绝绝对路径、`..` 与符号链接逃逸。
- Web 仅允许 HTTP/HTTPS；每次重定向均校验地址，默认拒绝 loopback、私网、链路本地、未指定和多播地址。
- 策略顺序：硬安全/mode 限制 -> browser/MCP 专用规则 -> `security::classify_tool` -> `[permissions.tools]` 精确名或 `*` 覆盖。
- `build` 可使用完整工具集但危险操作仍审批；`plan`、`explore` 拒绝文件变更、终端和变更型 Git。
- 默认允许读取、搜索、`web_fetch`、`git_diff`；文件变更、命令、子 Agent 和变更/远程 Git 要求审批；未知工具拒绝。
- API Key 只来自 Provider 环境变量或服务名 `1h-agent` 的系统钥匙串。密钥不得写入 TOML、SQLite、日志、导出或模型上下文。
- Unix 外部命令使用独立进程组；Windows 通过 `taskkill /T /F` 清理进程树。新增外部进程必须支持超时、截断和取消。

## 4. 资源与轻量化预算

| 对象 | 当前实现上限/默认值 |
| --- | --- |
| App/模型事件 channel | 各 `128`；子 Agent 模型事件 `512` |
| 输入与历史 | 512 KiB；仅内存，最多 50 条 |
| 显示与对话窗口 | 1,000 条或约 2 MiB；200 项或约 1 MiB |
| 思考摘要 / `@` 文件 | 1,024 B；单文件 64 KiB、总计 256 KiB |
| 输出剪贴板 / 边缘滚动 | 单次文本最多 256 KiB；约 80 ms 自动滚动计时器仅在拖拽期间存在 |
| Agent/tool | 默认最多 8 turn；命令 60 秒；工具输出 1 MiB |
| Web | 最多 5 次重定向；连接 10 秒、请求 30 秒；抓取默认 10 MiB |
| 浏览器桥接 | 默认关闭；30 秒、2 MiB、空闲 30 秒；配置最大 3,600 秒、8 MiB |
| 子 Agent | 一层、接口 `max_turns` 1-3、输出最多 256 KiB；当前为一次受限流式请求 |

新增缓存、channel、索引、输出或并发前，必须定义容量、截断、取消与释放路径。未知模型不得假设上下文窗口；使用 `provider.context_window_tokens` 或 `src/config.rs` 的已知模型注册表。

## 5. Provider、配置与数据

- OpenAI 默认 Responses/`gpt-5-mini`；DeepSeek 默认 Responses/`deepseek-v4-flash`；Qwen 与 Volcano Ark 默认 Chat Completions；Custom 默认兼容 Chat。
- DeepSeek Responses 在 `native_web_search != "disabled"` 时发送 Provider 原生 `web_search`，同时排除本地同名 function tool；DeepSeek 不使用 `previous_response_id`。
- 非密钥配置顺序：内置默认值 -> TOML（`--config` 指定路径优先）-> `AGENT_API_BASE`、`AGENT_MODEL`、`AGENT_PROVIDER` 覆盖。`AGENT_DATA_DIR` 覆盖数据目录。
- 密钥顺序：Provider 专属环境变量/`AGENT_API_KEY` -> 系统钥匙串。完整变量与示例配置见 `config/config.example.toml`。
- 数据库为 `<data_dir>/agent.db`，启用 WAL 与外键。`sessions` 保存 workspace/mode/provider/head，`turns` 形成分支树，`messages` 保存类型化消息；删除为软删除。
- fork 不复制 Provider `response_id`；undo/redo 只移动 session head；新输入在 undo 后创建新分支。

## 6. 实施与验证

1. 先读取任务路由目标与现有测试；保留用户已有改动，不做无关重构。
2. 先实现边界与资源上限，再连接 UI/Provider；新配置必须有默认值、校验、示例和安全语义。
3. 为行为变更添加有针对性的单元或集成测试；跨平台进程、网络和存储变更需覆盖取消/超时/恢复。
4. 至少运行与改动匹配的检查：`cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-features`；release 影响再运行 `cargo build --release`。
5. CI 在 Linux/macOS/Windows 测试；推送 `main` 或 PR 触发 CI。推送新 `v*` tag 触发 release，生成 Linux/Windows/macOS 归档、`.deb`、`.msi` 与 `SHA256SUMS.txt`。
6. 发布前同步 `Cargo.toml` 与 `Cargo.lock` 版本，先提交并推送，再创建新 tag。不得移动或复用已成功发布的 tag。

## 7. 维护检查

- 新能力是否保持事件驱动、无空闲重绘、无无界 `Vec`/channel/输出？
- 输出选择是否保持单次剪贴板文本不超过 256 KiB，且边缘自动滚动计时器只在拖拽期间存在？
- 新工具是否经过 `ToolRegistry`、workspace/SSRF 校验、mode、审批和进程清理？
- 新 Provider 事件是否先规范化，并明确持久化与恢复语义？
- 新文档事实是否只在本文件出现一次，并能回溯到源码、示例配置或工作流？
