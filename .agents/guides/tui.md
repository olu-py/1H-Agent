# TUI 性能与交互维护指南

## 适用范围

TUI adapter 的职责边界：projection、输入、布局、渲染、滚动和复制；不承担任何业务核心职责。

## 入口

- `src/app.rs`：TUI 门面 `App` 持有 `AppHandle` 与事件循环；`deferred_redraw_timer`、输入/菜单；所有变更经 `AppHandle` 提交。
- `src/projection.rs`：`TuiSessionProjection` 的 revision、锚点和缓存失效。
- `src/home.rs`：启动首页状态、响应式布局、有限最近会话、选择菜单和命中矩形。
- `src/output.rs`：`LayoutLine`、`StyleRun`、visual line、选区和 hit-test。
- `src/ui.rs`：`render_visual_line`、Markdown、实时思考缓存和 footer 控件矩形。

## 不变量

- TUI 只负责 projection、输入、布局、渲染、滚动和复制；所有会话/命令/审批/取消/provider 操作一律调用 `AppHandle`，不触碰核心 `SessionRuntime`/Storage/Provider/审批 oneshot。
- `TuiSessionProjection` 只保存展示状态（entries/todos/busy/thinking 等），不保存核心 runtime 或 oneshot。
- 终态、`TranscriptInvalidated`、`ResyncRequired` 时从 snapshot/messages 重建展示状态，不依赖被淘汰或过期的本地增量；普通 snapshot 刷新不推进未消费的 `event_cursor`，只有首次连接、消费者滞后或 `ResyncRequired` 才建立新基线，避免跳过工具终态、思考屏障和正文 delta。
- 逻辑行保存文本和 `StyleRun` 字节区间；可见行只切片相交段，不从行头扫描或逐字素建 Span。
- Markdown/换行按 revision、宽度和展开状态缓存；摘要只失效目标条目，滚动/footer 不重解析正文。
- 实时思考分离动态标题与正文；追加只重排末尾，裁剪或改宽才全量重建；每帧只生成一次结果。
- 文本/reasoning delta 与滚轮共享固定 16 ms 帧；状态立即累计，后续事件不重置 deadline。
- 点击、键盘、审批、终态和 resize 即时绘制；live `Approval` 必须同步到 facade 的全局最旧审批并立即显示 modal，`ApprovalResolved` 清理匹配项后从 snapshot 收敛下一个审批，禁止只更新 session projection 导致 agent 隐形等待超时；工具开始/结束、联网搜索状态变化和本地 Shell 完成强制重绘，工具终态在下一轮模型事件到达前就必须可见；思考结束时摘要默认折叠、点击可重新展开；后台会话的 delta 与 todo 更新不重绘当前会话；边界无效滚动不安排帧。
- 任务浮窗锚定 viewport 右下角，在消息内容之后渲染为顶层窗口；todo 更新不失效消息布局；状态符命中宽度固定为符号宽度；全部完成后自动折叠；关闭只隐藏当前会话浮窗不删任务，后续 todo 更新重新显示。
- 编辑鼠标点击相关功能时，不得假定窗口内容行与业务项索引天然一致；省略提示、标题、边框、滚动、裁剪或换行都可能造成渲染行与命中行偏移，渲染与 hit-test 必须复用同一套行映射规则并覆盖偏移场景测试。
- 主界面和首页的 mode、Provider、模型控件只使用当帧完整可见文字生成独立矩形；分隔符不可点击，resize/重绘先清旧矩形。
- 首页菜单捕获键鼠和粘贴，支持 `Up`/`Down`/`Enter`/`Esc`；popup、模型列表和最近会话均受屏幕或固定上限约束。
- 选择和复制保持 UTF-8/字素、滚动锚点与逻辑换行；复制受 `MAX_CLIPBOARD_BYTES` 限制。
- 边缘滚动 timer 只在拖选且方向非零时存在，松开或离开边缘立即释放。
- 启动首页不启动引擎 timer；核心引擎在 `AppService::start` 一次拉起，最近会话必须使用带硬上限的快照查询，确认新建或恢复后才初始化主界面资源。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 滚动不跟手 | 逐事件 draw -> deadline 重置 -> layout rebuild -> 阻塞 send |
| 展开摘要卡顿 | 全局失效 -> 重复 parse -> 整行扫描 -> 同帧重复生成 |
| 空闲高 CPU | 无更新 timer -> 动画生命周期 -> 后台误 redraw |
| 点击错位 | 实际 Rect -> 裁剪/宽字符 -> scroll offset |
| 选择乱码/缺失 | byte/grapheme 边界 -> 反向选区 -> style run 拆分 |
| 展示与核心不一致 | 是否直读核心状态 -> 终态/`TranscriptInvalidated`/`ResyncRequired` 后是否重建 projection |

## 验证

- 迭代过滤器按改动选择：`home::tests`、`app::tests`、`ui::tests`、`output::tests`、`ui_layout::tests`。
- 性能回归优先使用具体行为名过滤器，避免迭代期重复运行整个模块。
- 完成阶段按根文档运行一次 lib 测试；事件、缓存或选择跨模块变更升级到完整测试和 Clippy。
