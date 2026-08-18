# TUI 性能与交互维护指南

仅在修改渲染、布局、长文本、滚动、输入合帧、鼠标命中或会话切换时读取本文件。

## 先看哪些符号

- `src/app.rs`：事件循环、`deferred_redraw_timer`、`schedule_deferred_redraw`、输入和菜单状态。
- `src/session.rs`：`SessionRuntime`、条目 revision、滚动锚点、缓存失效和 `scroll_messages`。
- `src/output.rs`：`LayoutLine`、`StyleRun`、visual line、选区、hit-test 和复制。
- `src/ui.rs`：`render_visual_line`、Markdown、实时思考缓存、footer 控件矩形和实际绘制。

## 必须保持的不变量

- 长逻辑行预处理为文本和 `StyleRun` 字节区间；可见 visual line 只二分并切片相交样式段，不能从逻辑行头逐字素扫描或为每个字素建 Span。
- Markdown 和换行布局按条目 revision、viewport 宽度和展开状态缓存。摘要展开只失效目标条目，滚动和 footer 更新不重新解析正文。
- 实时思考将动态标题与缓存正文分开；宽度不变且仅追加时只重排末尾受影响行，头部裁剪或改宽才全量重建。
- `update_message_layout` 每帧只生成一次实时思考结果，`draw_messages` 直接消费缓存。
- 模型文本/reasoning delta 与滚轮共享固定 16 ms 延迟帧；状态立即累计，后续事件不重置 deadline，等待帧期间事件循环继续收输入。
- 点击、键盘、审批、完成、错误和 resize 即时绘制，并取消重复延迟帧；后台会话 delta 不触发当前界面绘制。
- `scroll_messages` 返回状态是否实际变化；边界无效滚动不安排帧，滚轮每刻度仍移动一行且不加入惯性。
- Provider 和模型使用独立、由实际 footer layout 计算的命中矩形；分隔符不可点击，滚动/裁剪后坐标仍正确。
- 选择、复制、中文、组合字符和 Emoji 始终停在 UTF-8/字素边界；布局缓存不能改变滚动锚点和逻辑换行复制语义。
- 单次复制受 `clipboard::MAX_CLIPBOARD_BYTES` 限制；边缘自动滚动 timer 只在拖选且方向非零时存在，松开或离开边缘后立即释放。

## 常见故障顺序

- 滚动不跟手：确认滚轮未逐事件 draw -> deadline 未被重置 -> `scroll_messages` 未触发布局 rebuild -> event select 没有阻塞 send。
- 展开长摘要卡顿：检查是否全局 invalidate、重复 Markdown parse、每可见行扫描整段或实时思考同帧生成两次。
- 空闲仍高 CPU：检查无待更新时 timer 是否清空、动画是否仅在需要时存在、后台事件是否误请求 redraw。
- 点击错位：查看实际 `Rect`、裁剪宽度、宽字符列宽和 scroll offset；不要从标签字符串长度猜坐标。
- 选择乱码或复制不全：检查 byte/grapheme 边界、反向选区和 style run 跨界拆分。

## 最小验证命令

```bash
cargo test long_visual_line_renders_only_the_visible_style_slices
cargo test expanding_thinking_summaries_parses_only_each_target_once
cargo test live_thinking_layout_reuses_body_and_only_processes_appended_tail
cargo test deferred_redraw_deadline_is_shared_without_being_reset
cargo test mouse_wheel_moves_one_line_and_reuses_layout
cargo test provider_and_model_text_open_independent_pickers
```

事件循环、缓存结构或选择语义变化时，必须再运行完整 Clippy 与测试。
