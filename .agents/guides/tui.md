# TUI 性能与交互维护指南

## 适用范围

渲染、布局缓存、长文本、实时思考、重绘合帧、滚动、鼠标命中、选择和复制。

## 入口

- `src/app.rs`：事件循环、`deferred_redraw_timer`、输入/菜单；`session.rs`：revision、锚点和缓存失效。
- `src/output.rs`：`LayoutLine`、`StyleRun`、visual line、选区和 hit-test。
- `src/ui.rs`：`render_visual_line`、Markdown、实时思考缓存和 footer 控件矩形。

## 不变量

- 逻辑行保存文本和 `StyleRun` 字节区间；可见行只切片相交段，不从行头扫描或逐字素建 Span。
- Markdown/换行按 revision、宽度和展开状态缓存；摘要只失效目标条目，滚动/footer 不重解析正文。
- 实时思考分离动态标题与正文；追加只重排末尾，裁剪或改宽才全量重建；每帧只生成一次结果。
- 文本/reasoning delta 与滚轮共享固定 16 ms 帧；状态立即累计，后续事件不重置 deadline。
- 点击、键盘、审批、终态和 resize 即时绘制；后台 delta 不重绘当前会话；边界无效滚动不安排帧。
- Provider/模型使用实际 footer layout 的独立矩形；分隔符不可点击，禁止硬编码列数。
- 选择和复制保持 UTF-8/字素、滚动锚点与逻辑换行；复制受 `MAX_CLIPBOARD_BYTES` 限制。
- 边缘滚动 timer 只在拖选且方向非零时存在，松开或离开边缘立即释放。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| 滚动不跟手 | 逐事件 draw -> deadline 重置 -> layout rebuild -> 阻塞 send |
| 展开摘要卡顿 | 全局失效 -> 重复 parse -> 整行扫描 -> 同帧重复生成 |
| 空闲高 CPU | 无更新 timer -> 动画生命周期 -> 后台误 redraw |
| 点击错位 | 实际 Rect -> 裁剪/宽字符 -> scroll offset |
| 选择乱码/缺失 | byte/grapheme 边界 -> 反向选区 -> style run 拆分 |

## 验证

```bash
cargo test long_visual_line_renders_only_the_visible_style_slices
cargo test expanding_thinking_summaries_parses_only_each_target_once
cargo test live_thinking_layout_reuses_body_and_only_processes_appended_tail
cargo test deferred_redraw_deadline_is_shared_without_being_reset
cargo test mouse_wheel_moves_one_line_and_reuses_layout
cargo test provider_and_model_text_open_independent_pickers
```
