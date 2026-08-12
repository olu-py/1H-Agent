use ratatui::{layout::Rect, text::Line};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct LayoutLine {
    pub text: String,
    pub styled: Line<'static>,
    pub start: usize,
    pub interaction: Option<InteractionTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InteractionTarget {
    Tool(String),
    Thinking,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualLine {
    pub logical_line: usize,
    pub start: usize,
    pub end: usize,
    pub interaction: Option<InteractionTarget>,
    pub synthetic: bool,
    pub clickable_width: usize,
}

#[derive(Clone, Debug)]
pub struct MessageLayout {
    pub viewport: Rect,
    pub width: usize,
    pub scroll: usize,
    pub text: String,
    pub lines: Vec<LayoutLine>,
    pub visual_lines: Vec<VisualLine>,
    pub live_thinking_before: Option<usize>,
    pub live_thinking_rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputSelection {
    pub anchor: usize,
    pub active: usize,
    pub dragging: bool,
}

impl OutputSelection {
    pub fn new(offset: usize) -> Self {
        Self {
            anchor: offset,
            active: offset,
            dragging: true,
        }
    }

    pub fn range(self) -> Option<(usize, usize)> {
        let start = self.anchor.min(self.active);
        let end = self.anchor.max(self.active);
        (start < end).then_some((start, end))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeScroll {
    pub direction: i8,
    pub column: u16,
}

impl MessageLayout {
    pub fn new(lines: Vec<Line<'static>>, viewport: Rect, scroll: usize) -> Self {
        Self::new_with_live_thinking(lines, viewport, scroll, None)
    }

    pub fn new_with_live_thinking(
        lines: Vec<Line<'static>>,
        viewport: Rect,
        scroll: usize,
        live_thinking_before: Option<usize>,
    ) -> Self {
        Self::new_with_interactions(lines, vec![], viewport, scroll, live_thinking_before)
    }

    pub fn new_with_interactions(
        lines: Vec<Line<'static>>,
        interactions: Vec<Option<InteractionTarget>>,
        viewport: Rect,
        scroll: usize,
        live_thinking_before: Option<usize>,
    ) -> Self {
        let width = viewport.width.max(1) as usize;
        let mut text = String::new();
        let mut layout_lines = Vec::with_capacity(lines.len());
        for (index, styled) in lines.into_iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            let start = text.len();
            let line_text = styled
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            text.push_str(&line_text);
            layout_lines.push(LayoutLine {
                text: line_text,
                styled,
                start,
                interaction: interactions.get(index).cloned().flatten(),
            });
        }

        let live_thinking_rows = usize::from(live_thinking_before.is_some());
        let visual_lines = build_visual_lines(
            &layout_lines,
            width,
            live_thinking_before,
            live_thinking_rows,
        );
        let max_scroll = visual_lines.len().saturating_sub(viewport.height as usize);
        Self {
            viewport,
            width,
            scroll: scroll.min(max_scroll),
            text,
            lines: layout_lines,
            visual_lines,
            live_thinking_before,
            live_thinking_rows,
        }
    }

    pub fn max_scroll(&self) -> usize {
        self.visual_lines
            .len()
            .saturating_sub(self.viewport.height as usize)
    }

    pub fn update_viewport(&mut self, viewport: Rect) {
        debug_assert_eq!(self.width, viewport.width.max(1) as usize);
        self.viewport = viewport;
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn set_scroll(&mut self, scroll: usize) {
        self.scroll = scroll.min(self.max_scroll());
    }

    pub fn reflow(mut self, viewport: Rect) -> Self {
        let width = viewport.width.max(1) as usize;
        self.visual_lines = build_visual_lines(
            &self.lines,
            width,
            self.live_thinking_before,
            self.live_thinking_rows,
        );
        self.viewport = viewport;
        self.width = width;
        self.scroll = self.scroll.min(self.max_scroll());
        self
    }

    pub fn set_live_thinking_lines(&mut self, lines: &[String]) {
        if self.live_thinking_rows == lines.len() {
            for (index, visual) in self
                .visual_lines
                .iter_mut()
                .filter(|line| line.synthetic)
                .enumerate()
            {
                visual.clickable_width = if index == 0 {
                    lines
                        .first()
                        .map(|line| UnicodeWidthStr::width(line.as_str()).min(self.width))
                        .unwrap_or(0)
                } else {
                    0
                };
            }
            return;
        }
        self.visual_lines.retain(|line| !line.synthetic);
        if let Some(logical_line) = self.live_thinking_before {
            let insertion = self
                .visual_lines
                .iter()
                .position(|line| line.logical_line >= logical_line)
                .unwrap_or(self.visual_lines.len());
            for (live_row, line) in lines.iter().enumerate() {
                self.visual_lines.insert(
                    insertion + live_row,
                    VisualLine {
                        logical_line: logical_line.min(self.lines.len().saturating_sub(1)),
                        start: live_row,
                        end: live_row,
                        interaction: (live_row == 0).then_some(InteractionTarget::Thinking),
                        synthetic: true,
                        clickable_width: if live_row == 0 {
                            UnicodeWidthStr::width(line.as_str()).min(self.width)
                        } else {
                            0
                        },
                    },
                );
            }
        }
        self.live_thinking_rows = lines.len();
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn hit_test(&self, column: u16, row: u16) -> Option<usize> {
        if self.viewport.width == 0
            || self.viewport.height == 0
            || column < self.viewport.x
            || column >= self.viewport.right()
            || row < self.viewport.y
            || row >= self.viewport.bottom()
        {
            return None;
        }
        let visual_row = self.scroll + usize::from(row - self.viewport.y);
        self.position_at_visual_row(visual_row, column.saturating_sub(self.viewport.x) as usize)
    }

    pub fn interaction_at(&self, column: u16, row: u16) -> Option<InteractionTarget> {
        if self.viewport.width == 0
            || self.viewport.height == 0
            || column < self.viewport.x
            || column >= self.viewport.right()
            || row < self.viewport.y
            || row >= self.viewport.bottom()
        {
            return None;
        }
        let visual_row = self.scroll + usize::from(row - self.viewport.y);
        let visual = self.visual_lines.get(visual_row)?;
        let target = visual.interaction.clone()?;
        let local_column = usize::from(column - self.viewport.x);
        (local_column < visual.clickable_width).then_some(target)
    }

    pub fn position_at_visual_row(&self, visual_row: usize, column: usize) -> Option<usize> {
        let row = self.visual_lines.get(visual_row)?;
        if row.synthetic {
            return None;
        }
        let line = self.lines.get(row.logical_line)?;
        let local_start = row.start.saturating_sub(line.start);
        let local_end = row.end.saturating_sub(line.start);
        let slice = &line.text[local_start..local_end];
        let mut used = 0usize;
        for (offset, grapheme) in slice.grapheme_indices(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if grapheme_width == 0 {
                continue;
            }
            let next = used.saturating_add(grapheme_width);
            if column < next {
                return Some(line.start + local_start + offset);
            }
            used = next;
        }
        Some(row.end)
    }

    pub fn selected_text(&self, selection: OutputSelection) -> Option<&str> {
        let (start, end) = selection.range()?;
        self.text.get(start..end)
    }
}

fn build_visual_lines(
    lines: &[LayoutLine],
    width: usize,
    live_thinking_before: Option<usize>,
    live_thinking_rows: usize,
) -> Vec<VisualLine> {
    let mut visual_lines = Vec::new();
    let insertion = live_thinking_before.map(|index| index.min(lines.len()));
    for logical_line in 0..=lines.len() {
        if insertion == Some(logical_line) {
            visual_lines.extend((0..live_thinking_rows).map(|live_row| VisualLine {
                logical_line: logical_line.min(lines.len().saturating_sub(1)),
                start: live_row,
                end: live_row,
                interaction: (live_row == 0).then_some(InteractionTarget::Thinking),
                synthetic: true,
                clickable_width: 0,
            }));
        }
        if let Some(line) = lines.get(logical_line) {
            visual_lines.extend(wrap_line(logical_line, line, width));
        }
    }
    visual_lines
}

fn wrap_line(logical_line: usize, line: &LayoutLine, width: usize) -> Vec<VisualLine> {
    if matches!(line.interaction, Some(InteractionTarget::Tool(_))) {
        let mut used = 0usize;
        let mut end = 0usize;
        for (offset, grapheme) in line.text.grapheme_indices(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if used.saturating_add(grapheme_width) > width {
                break;
            }
            used = used.saturating_add(grapheme_width);
            end = offset + grapheme.len();
        }
        return vec![VisualLine {
            logical_line,
            start: line.start,
            end: line.start + end,
            interaction: line.interaction.clone(),
            synthetic: false,
            clickable_width: used,
        }];
    }
    if line.text.is_empty() {
        return vec![VisualLine {
            logical_line,
            start: line.start,
            end: line.start,
            interaction: line.interaction.clone(),
            synthetic: false,
            clickable_width: 0,
        }];
    }

    let mut rows = Vec::new();
    let mut row_start = 0usize;
    let mut row_width = 0usize;
    for (offset, grapheme) in line.text.grapheme_indices(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if row_width > 0 && row_width.saturating_add(grapheme_width) > width {
            rows.push(VisualLine {
                logical_line,
                start: line.start + row_start,
                end: line.start + offset,
                interaction: if rows.is_empty() {
                    line.interaction.clone()
                } else {
                    None
                },
                synthetic: false,
                clickable_width: 0,
            });
            row_start = offset;
            row_width = 0;
        }
        row_width = row_width.saturating_add(grapheme_width);
    }
    rows.push(VisualLine {
        logical_line,
        start: line.start + row_start,
        end: line.start + line.text.len(),
        interaction: if rows.is_empty() {
            line.interaction.clone()
        } else {
            None
        },
        synthetic: false,
        clickable_width: 0,
    });
    rows
}

#[cfg(test)]
mod tests {
    use ratatui::{
        layout::Rect,
        text::{Line, Span},
    };

    use super::*;

    fn layout(text: &str, width: u16) -> MessageLayout {
        let lines = text
            .split('\n')
            .map(|line| Line::from(Span::raw(line.to_owned())))
            .collect();
        MessageLayout::new(lines, Rect::new(2, 3, width, 5), 0)
    }

    #[test]
    fn copies_logical_newlines_without_wrap_newlines() {
        let layout = layout("abcdef", 3);
        assert_eq!(layout.text, "abcdef");
        assert_eq!(layout.visual_lines.len(), 2);
        assert_eq!(
            layout.selected_text(OutputSelection {
                anchor: 1,
                active: 5,
                dragging: false
            }),
            Some("bcde")
        );
    }

    #[test]
    fn hit_testing_keeps_utf8_and_grapheme_boundaries() {
        let text = "a中e\u{301}🙂";
        let layout = layout(text, 20);
        let is_grapheme_boundary = |offset| {
            offset == text.len()
                || text
                    .grapheme_indices(true)
                    .any(|(start, _)| start == offset)
        };
        for column in 0..=6 {
            let offset = layout.hit_test(2 + column, 3).unwrap();
            assert!(layout.text.is_char_boundary(offset));
            assert!(is_grapheme_boundary(offset));
        }
        assert_eq!(layout.hit_test(2, 3), Some(0));
        assert_eq!(layout.hit_test(3, 3), Some(1));
        assert_eq!(layout.hit_test(4, 3), Some(1));
        assert_eq!(layout.hit_test(5, 3), Some("a中".len()));
        assert_eq!(layout.hit_test(6, 3), Some("a中e\u{301}".len()));
        assert_eq!(layout.hit_test(7, 3), Some("a中e\u{301}".len()));
        assert_eq!(layout.hit_test(8, 3), Some("a中e\u{301}🙂".len()));
        assert_eq!(layout.hit_test(9, 3), Some("a中e\u{301}🙂".len()));
    }

    #[test]
    fn hit_testing_handles_reverse_and_blank_lines() {
        let layout = layout("first\n\nlast", 20);
        let start = layout.hit_test(2, 3).unwrap();
        let end = layout.hit_test(2, 5).unwrap();
        assert_eq!(&layout.text[start..end], "first\n\n");
    }

    #[test]
    fn height_changes_preserve_wrapping_and_width_changes_reflow() {
        let mut layout = layout("abcdef", 3);
        let original_lines = layout.lines.len();
        let original_visual_lines = layout.visual_lines.clone();
        layout.update_viewport(Rect::new(2, 3, 3, 2));
        assert_eq!(layout.lines.len(), original_lines);
        assert_eq!(layout.visual_lines, original_visual_lines);

        let layout = layout.reflow(Rect::new(2, 3, 6, 2));
        assert_eq!(layout.lines.len(), original_lines);
        assert_eq!(layout.visual_lines.len(), 1);
        assert_eq!(layout.text, "abcdef");
    }

    #[test]
    fn scrolled_selection_preserves_chinese_and_emoji_boundaries() {
        let mut layout = MessageLayout::new(
            ["top", "中🙂", "bottom"]
                .into_iter()
                .map(|line| Line::from(Span::raw(line.to_owned())))
                .collect(),
            Rect::new(2, 3, 20, 1),
            0,
        );
        layout.set_scroll(1);
        let chinese = layout.hit_test(2, 3).unwrap();
        let inside_chinese = layout.hit_test(3, 3).unwrap();
        let emoji = layout.hit_test(4, 3).unwrap();
        let end = layout.hit_test(6, 3).unwrap();
        assert_eq!(chinese, inside_chinese);
        assert_eq!(&layout.text[chinese..emoji], "中");
        assert_eq!(&layout.text[emoji..end], "🙂");
        assert_eq!(
            layout.selected_text(OutputSelection {
                anchor: chinese,
                active: end,
                dragging: false,
            }),
            Some("中🙂")
        );
    }

    fn interactive_layout(viewport: Rect) -> MessageLayout {
        MessageLayout::new_with_interactions(
            vec![
                Line::from("▸ 文件读取  src/app.rs  ✓"),
                Line::from("▸ 文件搜索  thinking  ✓"),
                Line::from("▸ 文件修改  src/ui.rs  ✓"),
            ],
            vec![
                Some(InteractionTarget::Tool("first".into())),
                Some(InteractionTarget::Tool("second".into())),
                Some(InteractionTarget::Tool("third".into())),
            ],
            viewport,
            0,
            None,
        )
    }

    #[test]
    fn interaction_checks_real_viewport_rows_columns_and_wide_text() {
        let viewport = Rect::new(30, 1, 80, 18);
        let layout = interactive_layout(viewport);
        assert_eq!(
            layout.interaction_at(30, 1),
            Some(InteractionTarget::Tool("first".into()))
        );
        assert_eq!(
            layout.interaction_at(30, 2),
            Some(InteractionTarget::Tool("second".into()))
        );
        assert_eq!(
            layout.interaction_at(30, 3),
            Some(InteractionTarget::Tool("third".into()))
        );
        assert_eq!(layout.interaction_at(30, 0), None);
        assert_eq!(layout.interaction_at(30, 4), None);

        let visible_width = layout.visual_lines[0].clickable_width as u16;
        assert_eq!(
            layout.interaction_at(30 + visible_width - 1, 1),
            Some(InteractionTarget::Tool("first".into()))
        );
        assert_eq!(layout.interaction_at(30 + visible_width, 1), None);
        assert_eq!(layout.interaction_at(29, 1), None);
        assert_eq!(layout.interaction_at(110, 1), None);
    }

    #[test]
    fn interaction_maps_scrolled_rows_and_clipped_tool_width() {
        let mut layout = interactive_layout(Rect::new(0, 5, 12, 1));
        layout.set_scroll(1);
        assert_eq!(
            layout.interaction_at(0, 5),
            Some(InteractionTarget::Tool("second".into()))
        );
        assert_eq!(layout.visual_lines[1].clickable_width, 12);
        assert_eq!(
            layout.interaction_at(11, 5),
            Some(InteractionTarget::Tool("second".into()))
        );
        assert_eq!(layout.interaction_at(12, 5), None);
    }

    #[test]
    fn synthetic_thinking_click_width_updates_without_reflow() {
        let mut layout = MessageLayout::new_with_live_thinking(
            vec![Line::from("正文")],
            Rect::new(30, 1, 40, 5),
            0,
            Some(0),
        );
        layout.set_live_thinking_lines(&["⠋ 思考中  正在分析项目".into()]);
        let width = UnicodeWidthStr::width("⠋ 思考中  正在分析项目") as u16;
        assert_eq!(
            layout.interaction_at(30 + width - 1, 1),
            Some(InteractionTarget::Thinking)
        );
        assert_eq!(layout.interaction_at(30 + width, 1), None);

        layout.set_live_thinking_lines(&["▾ ⠙ 思考中".into(), "  正在分析项目".into()]);
        assert_eq!(
            layout.interaction_at(30, 1),
            Some(InteractionTarget::Thinking)
        );
        assert_eq!(layout.interaction_at(30, 2), None);
    }
}
