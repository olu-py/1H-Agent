# 1H-Agent AI 维护协议

> 读取对象：维护和开发本仓库的 AI Agent。先读本文件，再按任务路由读取目标源码；不要为了背景一次读取整个仓库。

## 0. 稳定上下文

```text
project: 1H-Agent
meaning: 1H = 氕（protium），不是“一小时”
goal: 极致轻量化、高性能、权限感知的跨平台终端 Agent
runtime: 单个 Rust/Tokio 进程，Ratatui + Crossterm，SQLite/WAL
authority: 源码 > config/config.example.toml > .github/workflows > 本文件
scope: TUI、模型流、受控工具、会话（多会话后台并发）、AI 集群模式与跨平台发布
excluded: Web UI、内置浏览器、远程 MCP、动态插件、图片/语音能力
cache_rule: 稳定规则置前；任务事实按源码路由按需读取
```


### 0.1 五分钟上手

1. 先运行 `git status --short --branch`，确认用户已有改动；不要还原或覆盖不属于当前任务的文件。
2. 从 `src/main.rs` 进入 `app::run`。全局状态在 `App`，单会话状态在 `SessionRuntime`，模型/工具循环在 `AgentRunner`。
3. 用 `rg` 搜索目标符号、事件变体和配置字段，只读取定义、直接调用者和相邻测试；不要先通读整个 `src/`。
4. 跨模块行为优先沿这条链检查：`terminal event -> App -> SessionRuntime -> AgentRunner -> OpenAiClient/ToolRegistry -> Storage -> RoutedEvent`。
5. 修改事件、配置或持久化类型时，必须同时搜索全部构造点、match 分支、序列化/恢复路径和测试。

核心文件速查：

| 文件 | 主要职责 |
| --- | --- |
| `src/main.rs` / `src/app.rs` | 启动、全局 UI、事件循环、会话注册表、菜单与 Provider 切换 |
| `src/session.rs` | 单会话对话、显示条目、滚动、流式状态和 AgentEvent 处理 |
| `src/agent.rs` | 模型流、工具调用、审批、子 Agent 与集群批次 |
| `src/provider/openai.rs` | Chat/Responses 请求体、SSE 与 Provider 事件规范化 |
| `src/tools/mod.rs` / `src/security.rs` | 工具注册、执行、权限与 workspace/网络边界 |
| `src/config.rs` / `src/settings.rs` / `src/secrets.rs` | 配置、Provider profile、设置表单与钥匙串缓存 |
| `src/storage.rs` | SQLite 会话、turn/message、工具结果与 Provider response 状态 |
| `src/ui.rs` / `src/output.rs` | Ratatui 渲染、布局缓存、命中测试、选择与复制 |

## 1. 任务路由

| 任务 | 首读文件 | 关联文件 |
| --- | --- | --- |
| TUI、快捷键、渲染、会话切换 | `src/app.rs` | `src/input.rs`、`src/ui.rs`、`src/commands.rs`、`src/session.rs`、`src/model.rs` |
| 系统提示词、Agent loop、审批、子 Agent | `src/agent.rs` | `src/prompt.rs`、`src/provider/mod.rs` |
| Chat/Responses、SSE、DeepSeek 原生搜索 | `src/provider/openai.rs` | `src/provider/mod.rs`、`src/config.rs` |
| 文件、命令、Git、Web、browser/MCP | `src/tools/mod.rs` | `src/tools/*.rs`、`src/security.rs` |
| 路径边界、SSRF、工具默认策略 | `src/security.rs` | `src/tools/process.rs`、`src/tools/web.rs` |
| TOML、环境变量、Provider、资源默认值 | `src/config.rs` | `config/config.example.toml`、`src/secrets.rs` |
| 会话、分支、迁移、持久化 | `src/storage.rs` | `src/session.rs`、`src/provider/mod.rs` |
| CI、release、安装包 | `.github/workflows/ci.yml` | `.github/workflows/release.yml`、`Cargo.toml` |

任务只读取涉及的行和直接依赖。发现本文件与事实来源不一致时，以事实来源为准，并在同一改动中更新本文件。

## 2. 架构与数据流

```text
terminal event -> app::App
                     |-- current: SessionRuntime  +-> agent::AgentRunner -> provider::OpenAiClient
                     |-- background: SessionRuntime   |            |
                     |                               +-> ToolRegistry <-> ToolCall
                     |-- router channel (RoutedEvent) <-- agent events
                     |-- Storage(SQLite/WAL) <- events/results
                     |-- secrets(OS keyring)
```

- `app` 只维护全局 UI 状态、会话运行时注册表（`current`/`background`）与事件路由；`ui` 只渲染快照；`input` 只编辑 UTF-8 文本。
- `session::SessionRuntime` 持有单个会话的对话、显示条目、runner、流式/思考状态与滚动位置；切换/新建会话不打断后台 agent，结果留在各自会话。
- `agent` 负责默认无固定轮次但受执行预算和资源上限约束的模型循环、审批和工具结果；`provider` 负责协议/SSE 转换；`tools` 负责分发。
- 跨模块使用 `ConversationItem`、`ModelRequest`、`ModelEvent`、`ToolCall`、`ToolDefinition`、`Usage`，不要把 Provider 私有 JSON 泄漏到 UI。显示类型集中定义在 `src/model.rs`。
- 正常路径：输入 -> 可选 `@` context -> `ConversationItem` -> 流式 `ModelEvent` -> 文本/工具事件经路由 channel 按 `session_id` 回送到对应会话 -> 下一 turn。
- `Esc` 必须取消当前活跃会话的模型、等待审批和外部进程；恢复会话时按 `head_turn_id` 的父链读取可见消息。

## 3. 安全与权限

- workspace 在启动时 canonicalize。已有目标必须在根目录内；新目标必须验证 canonical parent；拒绝绝对路径、`..` 与符号链接逃逸。
- Web 仅允许 HTTP/HTTPS；每次重定向均校验地址，默认拒绝 loopback、私网、链路本地、未指定和多播地址。
- 策略顺序：硬安全/mode 限制 -> browser/MCP 专用规则 -> `security::classify_tool` -> `[permissions.tools]` 精确名或 `*` 覆盖。
- `build`、`cluster` 可使用完整工具集但危险操作仍审批；`plan`、`explore` 拒绝文件变更、终端和变更型 Git。
- 默认允许读取、搜索、`web_fetch`、`git_diff`；文件变更、命令、子 Agent 和变更/远程 Git 要求审批；未知工具拒绝。
- API Key 只来自 Provider 环境变量或服务名 `1h-agent` 的系统钥匙串。密钥不得写入 TOML、SQLite、日志、导出或模型上下文。
- Unix 外部命令使用独立进程组；Windows 通过 `taskkill /T /F` 清理进程树。新增外部进程必须支持超时、截断和取消。

## 4. 资源与轻量化预算

| 对象 | 当前实现上限/默认值 |
| --- | --- |
| 路由/会话/子 Agent 事件 channel | 路由 `256`；会话 agent `128`；子 Agent 模型事件 `512` |
| 输入与历史 | 512 KiB；仅内存，最多 50 条 |
| 显示与对话窗口 | 1,000 条或约 2 MiB；200 项或约 1 MiB |
| 思考摘要 / `@` 文件 | 思考摘要每条持久化 64 KiB（实时显示行 1,024 B）；单文件 64 KiB、总计 256 KiB |
| 输出剪贴板 / 边缘滚动 | 单次文本最多 256 KiB；约 80 ms 自动滚动计时器仅在拖拽期间存在 |
| Agent/tool | 无固定 turn 上限；命令 60 秒；工具输出 1 MiB |
| Web | 最多 5 次重定向；连接 10 秒、请求 30 秒；抓取默认 10 MiB |
| 浏览器桥接 | 默认关闭；30 秒、2 MiB、空闲 30 秒；配置最大 3,600 秒、8 MiB |
| 子 Agent | 一层、默认无固定 turn 上限；最多并行 4 个；主动执行预算 300 秒（排队与审批不计时）；输出默认 256 KiB、工具输出默认 128 KiB、子上下文默认 48 条/512 KiB |

新增缓存、channel、索引、输出或并发前，必须定义容量、截断、取消与释放路径。未知模型不得假设上下文窗口；使用 `provider.context_window_tokens` 或 `src/config.rs` 的已知模型注册表。

## 5. Provider、配置与数据

- OpenAI 默认 Responses/`gpt-5-mini`；DeepSeek 默认 Responses/`deepseek-v4-flash`；Qwen 与 Volcano Ark 默认 Chat Completions；Custom 默认兼容 Chat。
- DeepSeek Responses 在 `native_web_search != "disabled"` 时发送 Provider 原生 `web_search`，同时排除本地同名 function tool；DeepSeek 不使用 `previous_response_id`。
- 非密钥配置顺序：内置默认值 -> TOML（`--config` 指定路径优先）-> `AGENT_API_BASE`、`AGENT_MODEL`、`AGENT_PROVIDER` 覆盖。`AGENT_DATA_DIR` 覆盖数据目录。
- 密钥顺序：Provider 专属环境变量/`AGENT_API_KEY` -> 系统钥匙串。启动阶段用 `api_key_cached` 预热全部预设；运行期 Provider 切换、设置、会话恢复和跨 Provider 子 Agent 只能用 `api_key_cached_only`，不得在 UI/Agent 热路径重新访问钥匙串。新输入的密钥用 `store_api_key_cached` 同步更新系统钥匙串和进程缓存。
- 数据库为 `<data_dir>/agent.db`，启用 WAL 与外键。`sessions` 保存 workspace/mode/provider/model/parent_id/head；`turns` 形成分支树；`messages` 保存类型化消息；删除为软删除。子会话通过 `parent_id` 挂在父会话下，各自保存独立 provider/model。
- fork 不复制 Provider `response_id`；undo/redo 只移动 session head；新输入在 undo 后创建新分支。

### 5.1 Provider 与 Responses 协议不变量

- `Config.provider` 是当前激活连接，`Config.providers` 按 `ProviderPreset` 唯一保存完整连接档案；旧单一 `[provider]` 必须兼容迁移。API Key 不属于任何 TOML/SQLite 结构。
- 切换 Provider 或模型必须清理当前会话旧 `response_id`，不能把一个模型/端点的服务端状态交给另一个连接。
- 使用 `previous_response_id` 时，增量输入从最新用户消息开始，必须同时包含该消息之后的 `@` 文件上下文，不能简单取 `items.len() - 1`。
- 服务端状态过期或不兼容时，先清理持久化 response ID，再重放本地历史。重放必须经过 `replay_safe_items`：过滤没有对应 function call 的 tool output，并移除没有结果的残缺 call。
- 裁剪对话不能留下位于历史开头的孤立 `ToolOutput`。遇到 `No tool call found for tool output`，优先检查游标、call/output 配对、response ID 和 Responses 请求序列，不要先增加重试次数。

### 5.2 集群与审批不变量

- `max_parallel_children` 默认 4；同一父 `AgentRunner` 的子任务 clone 共享 `child_slots`，并将该父批次并发限制为 4；同一 App 内的所有 runtime 共享全局审批锁。排队发生在主动预算计时开始前。
- 子 Agent 默认 `max_turns = 0`（无固定轮次上限）；显式正数才是硬上限。默认 300 秒主动预算只累计模型请求和工具执行，等待并发槽、审批锁和用户审批不计时。
- 同一模型轮次的所有 `agent_spawn` 结果必须齐全后主 Agent 才能继续；每个子 Agent 完成时应立即持久化并更新其工具行。
- 机器状态固定为 `completed`、`failed`、`turn_limit`、`timed_out`、`cancelled`；中文只用于 `label()` 和 UI。非完成结果仍需携带可用的部分 output。
- 审批从全局最早待审批项展示并按 owner session 路由。尚未取得锁的状态是“等待审批槽”，取得锁后才是“等待用户审批”。
- 父任务取消必须覆盖排队、模型流、工具、审批槽和审批等待，释放 semaphore/lock，并可靠发送终态；不能因 bounded channel 暂满而永久留在“运行中”。

### 5.3 TUI 性能与交互不变量

- 长文本渲染按逻辑行和 `StyleRun` 字节区间切片；可见 visual line 不得从逻辑行开头逐字素扫描，也不得为每个字素创建 Span。
- Markdown/换行布局按条目 revision、viewport 宽度和展开状态缓存。摘要展开只失效目标条目；实时思考追加只重排受影响的末尾逻辑行。
- 模型文本/reasoning delta 与滚轮共享固定 16 ms 延迟绘制窗口；滚动状态立即累计，事件循环在等待帧时继续收输入。窗口不能被后续事件反复顺延。
- 点击、键盘、审批、完成、错误和 resize 仍即时绘制；到达滚动边界后的无效输入不安排帧。普通滚动不能重新解析 Markdown。
- 底栏 Provider 和模型文字使用独立命中矩形与菜单状态，分隔符不可点击；切换任一项后重建 runner 并清理旧 response ID。

## 6. 实施与验证

1. 先读取任务路由目标与现有测试；保留用户已有改动，不做无关重构。
2. 先实现边界与资源上限，再连接 UI/Provider；新配置必须有默认值、校验、示例和安全语义。
3. 为行为变更添加有针对性的单元或集成测试；跨平台进程、网络和存储变更需覆盖取消/超时/恢复。
4. 至少运行与改动匹配的检查：`cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features --locked -- -D warnings`、`cargo test --all-features --locked`；release 影响再运行 `cargo build --release --locked`。
5. CI 在 Linux/macOS/Windows 测试；推送 `main` 或 PR 触发 CI。推送新 `v*` tag 触发 release，生成 Linux/Windows/macOS 归档、`.deb`、`.msi` 与 `SHA256SUMS.txt`。
6. 发布前同步 `Cargo.toml` 与 `Cargo.lock` 版本，先提交并推送，再创建新 tag。不得移动或复用已成功发布的 tag。

### 6.1 发布检查清单

1. 创建 `.github/release-notes/vX.Y.Z.md`，并确认 `Cargo.toml` 与 `Cargo.lock` 的 `protium-agent` 版本完全一致。
2. 运行格式、Clippy、完整测试、release 构建和 `git diff --check`；本地 `target/` 被清理后首次构建会很慢，这是正常现象。
3. 提交并推送 `main`，用远端 SHA 确认推送真实生效后，再创建新的 annotated `vX.Y.Z` tag。不得把已有成功 tag 移到新提交。
4. tag 推送触发 `.github/workflows/release.yml`：版本校验 -> 三平台测试 -> 四目标归档 + DEB/MSI -> checksums -> GitHub Release。
5. 不要假定本机安装了 `gh`。仓库的正式发布由 Actions 完成；本地只负责提交、tag 和状态核验。
6. Git HTTPS 的 macOS `osxkeychain` 可能长时间无输出。不要把“命令退出/只显示 Pushing”当作成功；用远端 SHA 或 GitHub API 确认。不得打印、写文件或提交任何凭据。
7. 发布完成后确认 Release 不是 draft/prerelease，且包含 Linux/Windows/macOS 归档、`.deb`、`.msi`、`SHA256SUMS.txt`、`THIRD_PARTY_NOTICES.md`。

## 7. 维护检查

- 新能力是否保持事件驱动、无空闲重绘、无无界 `Vec`/channel/输出？
- 输出选择是否保持单次剪贴板文本不超过 256 KiB，且边缘自动滚动计时器只在拖拽期间存在？
- 新工具是否经过 `ToolRegistry`、workspace/SSRF 校验、mode、审批和进程清理？
- 新 Provider 事件是否先规范化，并明确持久化与恢复语义？
- 新文档事实是否只在本文件出现一次，并能回溯到源码、示例配置或工作流？

### 7.1 高效诊断顺序

- 事件循环卡顿：先看 channel send 是否阻塞、是否逐事件 `terminal.draw`、是否重复布局/Markdown 解析、后台事件是否误触发当前会话重绘。
- Future/取消卡住：逐项检查 queue、semaphore、model stream、tool future、approval lock、oneshot sender 和 Drop guard 是否都有释放及终态路径。
- Provider 400：先打印/测试规范化后的请求结构（不得含密钥），核对 Chat/Responses 类型、call ID 配对、增量游标和 response ID 生命周期。
- Provider 配置丢失：核对 profile 迁移、`upsert_provider`、当前激活副本、session provider/model 和 runner rebuild；不要回退到预设 URL 掩盖已保存配置。
- UI 点击错位：坐标必须从实际裁剪后的 footer/layout 计算；文字、分隔符和菜单分别做命中测试，不能靠硬编码列数。
- 清理目录：`target/`、`.DS_Store` 等生成物可清理；数据库、配置、release notes、未跟踪源码和用户改动不可凭文件名猜测删除。先用 `git status --ignored` 与 `du` 确认。
