use ratatui::{layout::Rect, text::Line};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct LayoutLine {
    pub text: String,
    pub styled: Line<'static>,
    pub start: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisualLine {
    pub logical_line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct MessageLayout {
    pub viewport: Rect,
    pub width: usize,
    pub scroll: usize,
    pub text: String,
    pub lines: Vec<LayoutLine>,
    pub visual_lines: Vec<VisualLine>,
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
            });
        }

        let visual_lines: Vec<VisualLine> = layout_lines
            .iter()
            .enumerate()
            .flat_map(|(logical_line, line)| wrap_line(logical_line, line, width))
            .collect();
        let max_scroll = visual_lines.len().saturating_sub(viewport.height as usize);
        Self {
            viewport,
            width,
            scroll: scroll.min(max_scroll),
            text,
            lines: layout_lines,
            visual_lines,
        }
    }

    pub fn max_scroll(&self) -> usize {
        self.visual_lines
            .len()
            .saturating_sub(self.viewport.height as usize)
    }

    pub fn hit_test(&self, column: u16, row: u16) -> Option<usize> {
        if self.viewport.height == 0 || row < self.viewport.y || row >= self.viewport.bottom() {
            return None;
        }
        let visual_row = self.scroll + usize::from(row - self.viewport.y);
        self.position_at_visual_row(visual_row, column.saturating_sub(self.viewport.x) as usize)
    }

    pub fn position_at_visual_row(&self, visual_row: usize, column: usize) -> Option<usize> {
        let row = self.visual_lines.get(visual_row)?;
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

fn wrap_line(logical_line: usize, line: &LayoutLine, width: usize) -> Vec<VisualLine> {
    if line.text.is_empty() {
        return vec![VisualLine {
            logical_line,
            start: line.start,
            end: line.start,
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
}
