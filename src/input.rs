use unicode_width::UnicodeWidthChar;

pub use protium_core::input::InputBuffer;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InputViewport {
    pub text: String,
    pub cursor_column: usize,
    pub cursor_row: u16,
}

/// Computes the visible slice of `input` that fits `inner_width` columns, used
/// by the TUI renderer to keep the cursor row inside the input area.
pub(crate) fn input_viewport(input: &str, inner_width: usize) -> (&str, usize) {
    let text_capacity = inner_width.saturating_sub(1);
    let mut visible_width = 0;
    let mut start = input.len();
    for (index, character) in input.char_indices().rev() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if visible_width + character_width > text_capacity {
            break;
        }
        visible_width += character_width;
        start = index;
    }
    (&input[start..], visible_width)
}

pub(crate) fn input_cursor_viewport(
    input: &str,
    cursor: usize,
    inner_width: usize,
) -> InputViewport {
    let cursor = cursor.min(input.len());
    let line_start = input[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_row = input[..cursor]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u16;
    let line = &input[line_start..];
    let (visible_line, column) =
        input_viewport(&line[..cursor.saturating_sub(line_start)], inner_width);
    let mut text = input[line_start..]
        .lines()
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        text = visible_line.to_owned();
    }
    InputViewport {
        text,
        cursor_column: column,
        cursor_row: line_row.min(2),
    }
}

#[cfg(test)]
mod tests {
    use super::input_cursor_viewport;

    #[test]
    fn cursor_viewport_handles_wide_multiline_input() {
        let input = "first\n中文🙂tail";
        let cursor = "first\n中文🙂".len();
        let viewport = input_cursor_viewport(input, cursor, 7);

        assert_eq!(viewport.cursor_row, 1);
        assert_eq!(viewport.cursor_column, 6);
        assert!(viewport.text.contains("中文🙂tail"));
    }
}
