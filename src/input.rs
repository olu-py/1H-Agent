use std::collections::VecDeque;

/// A small, allocation-conscious terminal input buffer.
///
/// Crossterm reports cursor positions in terminal events, while Rust strings
/// use byte offsets.  Keeping the cursor on a UTF-8 boundary lets us support
/// CJK input without pulling in a full text-editor dependency.
#[derive(Clone, Debug)]
pub struct InputBuffer {
    text: String,
    cursor: usize,
    selection: Option<(usize, usize)>,
    history: VecDeque<String>,
    history_cursor: Option<usize>,
    max_history: usize,
    max_bytes: usize,
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            selection: None,
            history: VecDeque::with_capacity(50),
            history_cursor: None,
            max_history: 50,
            max_bytes: 512 * 1024,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        self.selection
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.selection = None;
        self.history_cursor = None;
    }

    pub fn set(&mut self, value: impl Into<String>) {
        self.text = value.into();
        if self.text.len() > self.max_bytes {
            self.text.truncate(self.max_bytes);
            while !self.text.is_char_boundary(self.text.len()) {
                self.text.pop();
            }
        }
        self.cursor = self.text.len();
        self.selection = None;
        self.history_cursor = None;
    }

    pub fn insert(&mut self, character: char) {
        self.delete_selection();
        let len = character.len_utf8();
        if self.text.len().saturating_add(len) > self.max_bytes {
            return;
        }
        self.text.insert(self.cursor, character);
        self.cursor += len;
        self.history_cursor = None;
    }

    pub fn insert_str(&mut self, value: &str) {
        self.delete_selection();
        let remaining = self.max_bytes.saturating_sub(self.text.len());
        if remaining == 0 {
            return;
        }
        let mut end = value.len().min(remaining);
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.insert_str(self.cursor, &value[..end]);
        self.cursor += end;
        self.history_cursor = None;
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let start = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.drain(start..self.cursor);
        self.cursor = start;
        self.history_cursor = None;
    }

    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor >= self.text.len() {
            return;
        }
        let end = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
        self.text.drain(self.cursor..end);
        self.history_cursor = None;
    }

    pub fn move_left(&mut self) {
        self.selection = None;
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }

    pub fn move_right(&mut self) {
        self.selection = None;
        self.cursor = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
    }

    pub fn move_home(&mut self) {
        self.selection = None;
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    pub fn move_end(&mut self) {
        self.selection = None;
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|index| self.cursor + index)
            .unwrap_or(self.text.len());
    }

    pub fn move_up(&mut self) {
        self.selection = None;
        let Some(line_start) = self.text[..self.cursor].rfind('\n') else {
            return;
        };
        let current_start = line_start + 1;
        let column = self.cursor.saturating_sub(current_start);
        let previous_end = line_start;
        let previous_start = self.text[..previous_end].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = (previous_start + column).min(previous_end);
    }

    pub fn move_down(&mut self) {
        self.selection = None;
        let current_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let current_end = self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len());
        if current_end == self.text.len() {
            return;
        }
        let column = self.cursor.saturating_sub(current_start);
        let next_start = current_end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(self.text.len());
        self.cursor = (next_start + column).min(next_end);
    }

    pub fn delete_word_left(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let before = &self.text[..self.cursor];
        let mut start = self.cursor;
        let mut seen_non_space = false;
        for (index, character) in before.char_indices().rev() {
            if character.is_whitespace() && seen_non_space {
                break;
            }
            if !character.is_whitespace() {
                seen_non_space = true;
            }
            start = index;
        }
        self.text.drain(start..self.cursor);
        self.cursor = start;
        self.history_cursor = None;
    }

    pub fn select_left(&mut self) {
        let anchor =
            self.selection.map_or(
                self.cursor,
                |(start, end)| if self.cursor == end { start } else { end },
            );
        self.selection = None;
        self.move_left();
        self.selection = Some((anchor.min(self.cursor), self.cursor.max(anchor)));
    }

    pub fn select_right(&mut self) {
        let anchor =
            self.selection.map_or(
                self.cursor,
                |(start, end)| if self.cursor == start { end } else { start },
            );
        self.selection = None;
        self.move_right();
        self.selection = Some((anchor.min(self.cursor), self.cursor.max(anchor)));
    }

    pub fn select_all(&mut self) {
        self.selection = Some((0, self.text.len()));
        self.cursor = self.text.len();
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection.take() else {
            return false;
        };
        let start = start.min(end);
        let end = end.max(start);
        if start == end {
            return false;
        }
        self.text.drain(start..end);
        self.cursor = start;
        self.history_cursor = None;
        true
    }

    pub fn push_history(&mut self) {
        let value = self.text.trim();
        if value.is_empty() {
            return;
        }
        if self.history.back().is_some_and(|item| item == value) {
            self.history_cursor = None;
            return;
        }
        self.history.push_back(value.to_owned());
        while self.history.len() > self.max_history {
            self.history.pop_front();
        }
        self.history_cursor = None;
    }

    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = self
            .history_cursor
            .map_or(self.history.len().saturating_sub(1), |index| {
                index.saturating_sub(1)
            });
        self.history_cursor = Some(next);
        if let Some(value) = self.history.get(next).cloned() {
            self.set(value);
            self.history_cursor = Some(next);
        }
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.history_cursor = None;
            self.clear();
            return;
        }
        let next = index + 1;
        self.history_cursor = Some(next);
        if let Some(value) = self.history.get(next).cloned() {
            self.set(value);
            self.history_cursor = Some(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::InputBuffer;

    #[test]
    fn edits_utf8_without_splitting_characters() {
        let mut input = InputBuffer::new();
        input.insert_str("中文a");
        input.move_left();
        input.backspace();
        assert_eq!(input.as_str(), "中a");
        input.move_home();
        input.insert('你');
        assert_eq!(input.as_str(), "你中a");
    }

    #[test]
    fn history_is_bounded_and_round_trips() {
        let mut input = InputBuffer::new();
        input.set("one");
        input.push_history();
        input.set("two");
        input.push_history();
        input.clear();
        input.history_previous();
        assert_eq!(input.as_str(), "two");
        input.history_previous();
        assert_eq!(input.as_str(), "one");
        input.history_next();
        assert_eq!(input.as_str(), "two");
        input.history_next();
        assert!(input.is_empty());
    }

    #[test]
    fn vertical_motion_stays_safe_on_single_line() {
        let mut input = InputBuffer::new();
        input.insert_str("single line");
        input.move_up();
        assert_eq!(input.cursor(), input.as_str().len());
    }

    #[test]
    fn selection_replaces_text_without_splitting_utf8() {
        let mut input = InputBuffer::new();
        input.insert_str("中文abc");
        input.select_left();
        input.select_left();
        input.insert_str("XY");
        assert_eq!(input.as_str(), "中文aXY");
        assert_eq!(input.selection(), None);
    }
}
