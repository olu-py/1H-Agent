# 1H-Agent

`1H` 指氕（protium），即氢-1 同位素。1H-Agent 是面向 Linux、macOS 和 Windows 的轻量、权限感知终端 Agent：单 Rust 二进制、流式对话、工具审批与本地会话持久化。

## 获取与启动

GitHub Releases 提供 Linux x86_64、Windows x86_64、macOS Intel 和 macOS Apple Silicon 的原生包，并附带 `SHA256SUMS.txt` 用于校验。

Windows 解压后在 PowerShell 运行：

```powershell
.\1h-agent.exe --workspace C:\path\to\project
```

macOS 请按芯片选择 `macos-aarch64` 或 `macos-x86_64`，解压后运行：

```bash
./1h-agent --workspace /path/to/project
```

当前 macOS 二进制未签名。若系统阻止首次运行，确认文件来源后可移除下载隔离属性：

```bash
xattr -d com.apple.quarantine ./1h-agent
```

从源码开发运行：

```bash
cargo run -- --workspace /path/to/project
```

构建 release 二进制：

```bash
cargo build --release
./target/release/1h-agent --workspace /path/to/project
```

## 配置 Provider

按 `Ctrl+S` 打开 Provider 设置。首页列出已保存的供应商连接和当前连接；选择“添加供应商”后，可从尚未添加的 OpenAI、DeepSeek、Qwen/Bailian、火山方舟和自定义兼容模板中创建连接。每种模板只能添加一次，选择已有连接可编辑并切换，`Ctrl+D` 可移除连接。非密钥配置保存到 TOML；API Key 仍保存到系统钥匙串，移除连接不会删除密钥，钥匙串不可用时密钥仅保留到当前进程结束。

| Provider | API Key 环境变量 | 默认模型 |
| --- | --- | --- |
| OpenAI | `OPENAI_API_KEY` | `gpt-5-mini` |
| DeepSeek | `DEEPSEEK_API_KEY` | `deepseek-v4-flash` |
| Qwen/Bailian | `DASHSCOPE_API_KEY` | `qwen3.8-max` |
| Volcano Ark | `ARK_API_KEY` | `doubao-seed-2-1-pro-260628` |
| Custom | `AGENT_API_KEY` | 自行设置 |

配置示例见 [`config/config.example.toml`](config/config.example.toml)。`AGENT_API_BASE`、`AGENT_MODEL`、`AGENT_PROVIDER` 可覆盖 Provider 字段，`AGENT_DATA_DIR` 可指定会话数据库目录。Qwen/Bailian 的 URL 必须替换其中的 `WorkspaceId`。

DeepSeek 的 Responses 模式默认启用 Provider 原生联网搜索。设置以下配置可关闭它，并回退到本地文本搜索与网页抓取：

```toml
[provider]
native_web_search = "disabled"
```

## 常用操作

| 操作 | 快捷键/语法 |
| --- | --- |
| 发送 / 换行 | `Enter` / `Shift+Enter` 或 `Ctrl+J` |
| 新会话 / 切换会话 | `Ctrl+N` / `Alt+Up`、`Alt+Down` / 点击会话行（父会话点击展开/收起子会话） |
| 切换模式 | `Ctrl+X` 后按 `m` / `/plan` `/build` `/explore` `/cluster` / 点击输入框标题的模式标签 |
| 命令面板 / 命令 | `Ctrl+P` / `/` |
| 选择模型 | 点击输入框下方的“Provider · 模型” / `/model <模型名>` |
| 引用文件 / 执行命令 | `@path` / `!command`（命令须审批） |
| 滚动 / 回到底部 | `PageUp`、`PageDown` / `Ctrl+L` |
| 输出选择 / 复制 | 在任务输出区按住鼠标左键拖选，松开后自动复制 |
| 粘贴 | 由终端环境决定（例如 `Cmd+V`、`Ctrl+Shift+V`） |
| 工具详情 / 审批 | 鼠标点击工具摘要 / `Y`、`N` |
| 取消 / 退出 | `Esc` / `Ctrl+C` |

> `Alt+Up` / `Alt+Down` 依赖 kitty keyboard protocol，请使用支持该协议的终端（如 kitty、WezTerm、Alacritty、foot、iTerm2）。不支持的老终端（例如 macOS 自带 Terminal.app）可能无法区分 `Alt+方向键` 与裸方向键。

文件操作限定在 `--workspace` 内；写入、删除、命令、浏览器交互和变更型 Git 操作会按策略要求审批。

## AI 集群模式

切换到 `cluster` 模式（`/cluster` 或输入框模式标签）后，可在对话里用自然语言给不同角色指派不同模型，例如「用 deepseek-v4-pro 做计划与审批，用 deepseek-v4-flash 做实施」。主 Agent 会通过 `agent_spawn` 调度子 Agent 串行/并行执行，每个子 Agent 生成一个**树形子会话**（默认折叠，点击展开），父会话与当前会话在左侧面板高亮显示，子会话会显示运行中/等待审批等状态。子 Agent 返回 JSON 结果（`session_id`、`status`、`output`），写文件操作仍需用户审批；子 Agent 无终端权限，验证由主 Agent 完成。`agent_spawn` 还可通过 `provider` 指定其他 Provider、通过 `agent` 引用 `[[agents]]` 配置模板。

## AI 维护文档

维护或开发本项目的 AI Agent 请读取 [AGENT.md](AGENT.md)。该文档提供架构、源码路由、安全边界、资源上限、验证和发布规则。

## 第三方声明

本项目使用或改写的第三方内容及其许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
