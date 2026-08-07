use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, DisplayContent, DisplayKind, SettingsField, SettingsState};

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let horizontal = if area.width >= 90 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(40)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(0), Constraint::Min(1)])
            .split(area)
    };

    if area.width >= 90 {
        draw_sessions(frame, horizontal[0], app);
    }
    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(horizontal[1]);
    draw_messages(frame, main[0], app);
    draw_input(frame, main[1], app);
    draw_status(frame, main[2], app);
    draw_help(frame, main[3], app);
    if app.pending_approval.is_some() {
        draw_approval(frame, area, app);
    }
    if let Some(settings) = &app.settings {
        draw_settings(frame, area, settings);
    }
}

fn draw_sessions(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let workspace = app
        .workspace
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| app.workspace.to_string_lossy());
    let content_width = area.width.saturating_sub(3) as usize;
    let mut items = vec![
        ListItem::new(Line::from(Span::styled(
            "Alt+Up/Down  switch",
            Style::default().fg(Color::DarkGray),
        ))),
        ListItem::new(Line::from(Span::styled(
            "Ctrl+N       new",
            Style::default().fg(Color::DarkGray),
        ))),
        ListItem::new(Line::default()),
        ListItem::new(Line::from(Span::styled(
            fit_text(&workspace, content_width),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))),
    ];
    let visible_sessions = area.height.saturating_sub(6) as usize;
    let current = app
        .sessions
        .iter()
        .position(|session| session.id == app.session_id)
        .unwrap_or(0);
    let start = current
        .saturating_add(1)
        .saturating_sub(visible_sessions)
        .min(app.sessions.len().saturating_sub(visible_sessions));
    for session in app.sessions.iter().skip(start).take(visible_sessions) {
        let active = session.id == app.session_id;
        let marker = if active { "> " } else { "  " };
        let style = if active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "{marker}{}",
                fit_text(&session.title, content_width.saturating_sub(2))
            ),
            style,
        ))));
    }
    frame.render_widget(
        List::new(items).block(Block::default().title(" Sessions ").borders(Borders::RIGHT)),
        area,
    );
}

fn draw_messages(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    for entry in &app.entries {
        let (label, color) = match &entry.kind {
            DisplayKind::User => ("You", Color::Green),
            DisplayKind::Assistant => ("Agent", Color::Cyan),
            DisplayKind::Tool => ("Tool", Color::Yellow),
            DisplayKind::Error => ("Error", Color::Red),
            DisplayKind::System => ("System", Color::DarkGray),
        };
        lines.push(Line::from(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        let content_style = Style::default().fg(color);
        match &entry.content {
            DisplayContent::Markdown(text) => {
                if text.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "...",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    lines.extend(render_markdown(text, content_style));
                }
            }
            DisplayContent::ToolCall { name, arguments } => {
                lines.extend(render_tool_call(name, arguments));
            }
            DisplayContent::ToolResult { name, result } => {
                lines.extend(render_tool_result(name, result));
            }
        }
        lines.push(Line::default());
    }
    let visible_height = area.height.saturating_sub(1) as usize;
    let scroll = wrapped_height(&lines, area.width as usize)
        .saturating_sub(visible_height)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(" Task ").borders(Borders::BOTTOM))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn render_markdown(text: &str, base: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = raw.trim_start();
        if is_code_fence(trimmed) {
            let marker = if in_code {
                "[end code]".to_owned()
            } else {
                let language = trimmed[3..].trim();
                if language.is_empty() {
                    "[code]".to_owned()
                } else {
                    format!("[code: {language}]")
                }
            };
            in_code = !in_code;
            lines.push(Line::from(Span::styled(marker, code_fence_style())));
        } else if in_code {
            lines.push(Line::from(Span::styled(raw.to_owned(), code_style())));
        } else {
            lines.push(render_markdown_line(raw, base));
        }
    }
    lines
}

fn render_markdown_line(raw: &str, base: Style) -> Line<'static> {
    let trimmed = raw.trim_start();
    let indent = &raw[..raw.len().saturating_sub(trimmed.len())];
    if trimmed.is_empty() {
        return Line::default();
    }
    if let Some((level, body)) = heading_parts(trimmed) {
        let heading_style = base
            .fg(if level <= 2 { Color::Cyan } else { Color::Blue })
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled(indent.to_owned(), heading_style)];
        spans.extend(render_inline(body, heading_style));
        return Line::from(spans);
    }
    if let Some(body) = unordered_list_body(trimmed) {
        let marker_style = base.fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled(format!("{indent}• "), marker_style)];
        spans.extend(render_inline(body, base));
        return Line::from(spans);
    }
    if let Some((marker, body)) = ordered_list_parts(trimmed) {
        let marker_style = base.fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled(format!("{indent}{marker} "), marker_style)];
        spans.extend(render_inline(body, base));
        return Line::from(spans);
    }
    if let Some(body) = trimmed.strip_prefix('>') {
        let body = body.strip_prefix(' ').unwrap_or(body);
        let quote_style = base.fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
        let mut spans = vec![Span::styled(format!("{indent}│ "), quote_style)];
        spans.extend(render_inline(body, quote_style));
        return Line::from(spans);
    }
    if is_horizontal_rule(trimmed) {
        return Line::from(Span::styled(raw.to_owned(), base.fg(Color::DarkGray)));
    }
    Line::from(render_inline(raw, base))
}

fn render_inline(value: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain_start = 0;
    let mut index = 0;
    while index < value.len() {
        let remainder = &value[index..];
        if let Some(stripped) = remainder.strip_prefix('[') {
            if let Some(label_end) = stripped.find("](") {
                let label_end = index + 1 + label_end;
                let url_start = label_end + 2;
                if let Some(url_end) = value[url_start..].find(')') {
                    let url_end = url_start + url_end;
                    if url_end > url_start {
                        push_plain(&mut spans, value, plain_start, index, base);
                        let link_style = base.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED);
                        spans.push(Span::styled(
                            value[index + 1..label_end].to_owned(),
                            link_style,
                        ));
                        spans.push(Span::styled(
                            format!(" ({})", &value[url_start..url_end]),
                            base.fg(Color::DarkGray),
                        ));
                        index = url_end + 1;
                        plain_start = index;
                        continue;
                    }
                }
            }
        }

        let marker = if remainder.starts_with("**") || remainder.starts_with("__") {
            Some((2, &remainder[..2], base.add_modifier(Modifier::BOLD)))
        } else if remainder.starts_with("~~") {
            Some((2, "~~", base.add_modifier(Modifier::CROSSED_OUT)))
        } else if remainder.starts_with('`') {
            Some((1, "`", code_style()))
        } else if remainder.starts_with('*') || remainder.starts_with('_') {
            Some((1, &remainder[..1], base.add_modifier(Modifier::ITALIC)))
        } else {
            None
        };
        let Some((marker_len, marker, style)) = marker else {
            let character_len = remainder.chars().next().map(char::len_utf8).unwrap_or(1);
            index += character_len;
            continue;
        };
        let body_start = index + marker_len;
        if let Some(end_offset) = value[body_start..].find(marker) {
            let end = body_start + end_offset;
            if end > body_start {
                push_plain(&mut spans, value, plain_start, index, base);
                spans.push(Span::styled(value[body_start..end].to_owned(), style));
                index = end + marker_len;
                plain_start = index;
                continue;
            }
        }
        index += marker_len;
    }
    push_plain(&mut spans, value, plain_start, value.len(), base);
    spans
}

fn push_plain(spans: &mut Vec<Span<'static>>, value: &str, start: usize, end: usize, style: Style) {
    if start < end {
        spans.push(Span::styled(value[start..end].to_owned(), style));
    }
}

fn render_tool_call(name: &str, arguments: &Value) -> Vec<Line<'static>> {
    let arguments =
        serde_json::to_string_pretty(arguments).unwrap_or_else(|_| arguments.to_string());
    let mut lines = vec![Line::from(vec![
        Span::styled("call ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            name.to_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    lines.push(Line::from(Span::styled(
        "arguments",
        Style::default().fg(Color::DarkGray),
    )));
    for line in arguments.lines() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(line.to_owned(), code_style()),
        ]));
    }
    lines
}

fn render_tool_result(name: &str, result: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("result ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            name.to_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    lines.push(Line::from(Span::styled(
        "output",
        Style::default().fg(Color::DarkGray),
    )));
    if result.is_empty() {
        lines.push(Line::from(Span::styled("  (empty)", code_style())));
    } else {
        lines.extend(result.lines().map(|line| {
            Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(line.to_owned(), code_style()),
            ])
        }));
    }
    lines
}

fn wrapped_height(lines: &[Line<'static>], width: usize) -> usize {
    let width = width.max(1);
    lines
        .iter()
        .map(|line| {
            let line_width = line
                .spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            line_width.max(1).div_ceil(width)
        })
        .sum()
}

fn heading_parts(value: &str) -> Option<(usize, &str)> {
    let level = value.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&level)
        && value
            .as_bytes()
            .get(level)
            .is_some_and(u8::is_ascii_whitespace)
    {
        Some((level, value[level..].trim_start()))
    } else {
        None
    }
}

fn unordered_list_body(value: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| value.strip_prefix(marker))
}

fn ordered_list_parts(value: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for (index, character) in value.char_indices() {
        if character.is_ascii_digit() {
            end = index + character.len_utf8();
        } else {
            break;
        }
    }
    if end == 0
        || value
            .as_bytes()
            .get(end)
            .is_none_or(|byte| *byte != b'.' && *byte != b')')
    {
        return None;
    }
    let body_start = end + 1;
    if !value
        .as_bytes()
        .get(body_start)
        .is_some_and(u8::is_ascii_whitespace)
    {
        return None;
    }
    Some((&value[..body_start], value[body_start..].trim_start()))
}

fn is_code_fence(value: &str) -> bool {
    value.starts_with("```") || value.starts_with("~~~")
}

fn is_horizontal_rule(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_')
        && value.chars().count() >= 3
        && value.chars().all(|character| character == first)
}

fn code_style() -> Style {
    Style::default().fg(Color::Yellow).bg(Color::Black)
}

fn code_fence_style() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let style = if app.busy {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };
    let inner_width = area.width.saturating_sub(2) as usize;
    let (visible_input, cursor_column) = input_viewport(&app.input, inner_width);
    frame.render_widget(
        Paragraph::new(visible_input)
            .style(style)
            .block(Block::default().title(" Input ").borders(Borders::ALL)),
        area,
    );
    if !app.busy && app.settings.is_none() && app.pending_approval.is_none() {
        let cursor_x = area.x + 1 + cursor_column as u16;
        frame.set_cursor_position((cursor_x, area.y + 1));
    }
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let tokens = if app.usage.total_tokens > 0 {
        format!(" | {} tokens", app.usage.total_tokens)
    } else {
        String::new()
    };
    frame.render_widget(
        Paragraph::new(format!("{}{}", app.status, tokens))
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (primary, sessions) = if app.settings.is_some() {
        (
            "Tab/Up/Down Field | Left/Right Change | Enter Save | Esc Cancel",
            "API Key is masked and never written to the config file",
        )
    } else if app.pending_approval.is_some() {
        ("Y Approve | N/Esc Reject | Ctrl+C Quit", "")
    } else if app.busy {
        ("Esc Cancel request | Ctrl+C Quit", "")
    } else {
        (
            "Enter Send | Ctrl+S Provider | Esc Cancel | Ctrl+C Quit",
            "Alt+Up/Down Switch session | Ctrl+N New session",
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Keys  ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(primary),
            ]),
            Line::from(format!("      {sessions}")),
        ])
        .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn draw_approval(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let popup = centered_rect(72, 12, area);
    let approval = app.pending_approval.as_ref().expect("approval exists");
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Tool: {}", approval.call.name),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(approval.reason.clone()),
            Line::from(""),
            Line::from(approval.call.arguments.to_string()),
            Line::from(""),
            Line::from(Span::styled(
                "Y approve    N reject",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
        ])
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Approval required ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn draw_settings(frame: &mut Frame<'_>, area: Rect, settings: &SettingsState) {
    let popup = centered_rect(84, 17, area);
    let key = if settings.api_key.is_empty() {
        if settings.has_existing_key {
            "********".to_owned()
        } else {
            "(not set)".to_owned()
        }
    } else {
        "*".repeat(settings.api_key.chars().count().min(32))
    };
    let values = [
        (
            SettingsField::Preset,
            "Provider",
            settings.provider.preset.label().to_owned(),
        ),
        (
            SettingsField::Protocol,
            "Protocol",
            settings.provider.kind.label().to_owned(),
        ),
        (
            SettingsField::BaseUrl,
            "Base URL",
            settings.provider.base_url.clone(),
        ),
        (
            SettingsField::Model,
            "Model",
            settings.provider.model.clone(),
        ),
        (SettingsField::ApiKey, "API Key", key),
    ];
    let value_width = popup.width.saturating_sub(19) as usize;
    let mut lines = vec![Line::default()];
    for (field, label, value) in values {
        let selected = settings.field == field;
        let marker = if selected { ">" } else { " " };
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} {label:<10}"), style),
            Span::styled(fit_text(&value, value_width), style),
        ]));
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        "Tab/Up/Down: field   Left/Right: change",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from("Enter: save   Esc: cancel"));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Provider settings ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn fit_text(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let target = width - 3;
    let mut used = 0;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > target {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output + "..."
}

fn input_viewport(input: &str, inner_width: usize) -> (&str, usize) {
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

fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(20)
        .min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_cursor_tracks_terminal_columns() {
        assert_eq!(input_viewport("hello", 20), ("hello", 5));
        assert_eq!(
            input_viewport("12345678901234567890", 20),
            ("2345678901234567890", 19)
        );
        assert_eq!(input_viewport("中文a", 20), ("中文a", 5));
        assert_eq!(input_viewport("中文测试abc", 8), ("测试abc", 7));
    }

    #[test]
    fn text_fitting_respects_wide_characters() {
        assert_eq!(fit_text("中文测试", 7), "中文...");
        assert_eq!(fit_text("short", 7), "short");
    }

    #[test]
    fn markdown_blocks_and_inline_styles_are_rendered() {
        let lines = render_markdown(
            "# Heading\n- **bold text** and `code`\n> quote\n```rust\nlet value = 1;\n```",
            Style::default(),
        );
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(text[0], "Heading");
        assert_eq!(text[1], "• bold text and code");
        assert_eq!(text[2], "│ quote");
        assert_eq!(text[3], "[code: rust]");
        assert_eq!(text[4], "let value = 1;");
        assert_eq!(text[5], "[end code]");
        assert!(lines[1].spans.len() >= 4);
    }

    #[test]
    fn tool_output_is_structured_and_pretty_printed() {
        let call = render_tool_call("file_read", &serde_json::json!({"path": "a.txt"}));
        let result = render_tool_result("file_read", "line one\nline two");
        assert!(
            call.iter()
                .any(|line| { line.spans.iter().any(|span| span.content == "file_read") })
        );
        assert!(call.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content == "  \"path\": \"a.txt\"")
        }));
        assert!(result.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                == "│ line two"
        }));
    }
}
