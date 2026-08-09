use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{
        AgentPhase, App, CommandPaletteState, DisplayContent, DisplayKind, ModelPhase,
        SettingsField, SettingsState,
    },
    commands,
};

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
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(horizontal[1]);
    draw_messages(frame, main[0], app);
    draw_input(frame, main[1], app);
    if !app.file_suggestions.is_empty() && app.palette.is_none() && app.settings.is_none() {
        draw_file_suggestions(frame, main[1], app);
    }
    draw_status(frame, main[2], app);
    draw_help(frame, main[3], app);
    if app.pending_approval.is_some() {
        draw_approval(frame, area, app);
    }
    if let Some(settings) = &app.settings {
        draw_settings(frame, area, settings);
    }
    if let Some(palette) = &app.palette {
        draw_palette(frame, area, palette);
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
            "Alt+Up/Down  切换会话",
            Style::default().fg(Color::DarkGray),
        ))),
        ListItem::new(Line::from(Span::styled(
            "Ctrl+N       新建会话",
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
        List::new(items).block(Block::default().title(" 会话 ").borders(Borders::RIGHT)),
        area,
    );
}

fn draw_messages(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    for (entry_index, entry) in app.entries.iter().enumerate() {
        let (label, color) = match &entry.kind {
            DisplayKind::User => ("用户", Color::Green),
            DisplayKind::Assistant => ("Agent", Color::Cyan),
            DisplayKind::Thinking => ("思考摘要", Color::Magenta),
            DisplayKind::Tool => ("工具", Color::Yellow),
            DisplayKind::Error => ("错误", Color::Red),
            DisplayKind::System => ("系统", Color::DarkGray),
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
            DisplayContent::Diff(diff) => {
                lines.extend(render_diff(diff));
            }
            DisplayContent::ToolCall { name, arguments } => {
                if app.expanded_tools.contains(&entry_index) {
                    lines.extend(render_tool_call(name, arguments));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("调用 {name} [已折叠；Ctrl+O 展开]"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            DisplayContent::ToolResult { name, result } => {
                if app.expanded_tools.contains(&entry_index) {
                    lines.extend(render_tool_result(name, result));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("结果 {name} [{} 字节；已折叠；Ctrl+O 展开]", result.len()),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }
        lines.push(Line::default());
    }
    let visible_height = area.height.saturating_sub(1) as usize;
    let max_scroll = wrapped_height(&lines, area.width as usize).saturating_sub(visible_height);
    let scroll = if app.follow_output {
        max_scroll
    } else {
        max_scroll.saturating_sub(app.message_scroll)
    }
    .min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(" 任务 ").borders(Borders::BOTTOM))
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
        Span::styled("调用 ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            name.to_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    lines.push(Line::from(Span::styled(
        "参数",
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
        Span::styled("结果 ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            name.to_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    lines.push(Line::from(Span::styled(
        "输出",
        Style::default().fg(Color::DarkGray),
    )));
    if result.is_empty() {
        lines.push(Line::from(Span::styled("  （空）", code_style())));
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

fn render_diff(diff: &str) -> Vec<Line<'static>> {
    if diff.trim().is_empty() {
        return vec![Line::from(Span::styled(
            "（工作区干净）",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    diff.lines()
        .map(|line| {
            let color = if line.starts_with("+++") || line.starts_with("---") {
                Color::Cyan
            } else if line.starts_with('+') {
                Color::Green
            } else if line.starts_with('-') {
                Color::Red
            } else if line.starts_with("@@") {
                Color::Yellow
            } else if line.starts_with("diff ") {
                Color::Cyan
            } else {
                Color::White
            };
            Line::from(Span::styled(line.to_owned(), Style::default().fg(color)))
        })
        .collect()
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
    let (visible_input, cursor_column, cursor_row) =
        input_cursor_viewport(app.input.as_str(), app.input.cursor(), inner_width);
    frame.render_widget(
        Paragraph::new(visible_input)
            .style(style)
            .block(Block::default().title(" 输入 ").borders(Borders::ALL)),
        area,
    );
    if !app.busy && app.settings.is_none() && app.pending_approval.is_none() {
        let cursor_x = area.x + 1 + cursor_column as u16;
        let cursor_y = area.y + 1 + cursor_row.min(area.height.saturating_sub(3));
        frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(1)), cursor_y));
    }
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let context_line = if app.context_meter_enabled {
        if let Some(limit) = app.context_limit_tokens {
            let used = app.context_used_tokens.min(limit.max(1));
            let percent = used.saturating_mul(100) / limit.max(1);
            let meter_color = if percent >= 95 {
                Color::Red
            } else if percent >= 85 {
                Color::LightRed
            } else if percent >= 70 {
                Color::Yellow
            } else {
                Color::Green
            };
            let hint = if percent >= 85 {
                "  建议执行 /compact"
            } else {
                ""
            };
            Line::from(vec![
                Span::styled(" 上下文窗口 ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    context_ring(percent),
                    Style::default()
                        .fg(meter_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {percent}%  {used}/{limit} tokens"),
                    Style::default()
                        .fg(meter_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(hint, Style::default().fg(Color::Yellow)),
            ])
        } else {
            Line::from(vec![
                Span::styled(" 上下文窗口 ", Style::default().fg(Color::DarkGray)),
                Span::styled("○ --%", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("  已用约 {} tokens / 上限未知", app.context_used_tokens),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }
    } else {
        Line::default()
    };
    let mode_color = match app.mode {
        commands::AgentMode::Build => Color::Green,
        commands::AgentMode::Plan => Color::Blue,
        commands::AgentMode::Explore => Color::Cyan,
    };
    let agent_color = match app.agent_phase {
        AgentPhase::Idle | AgentPhase::Completed => Color::DarkGray,
        AgentPhase::Thinking | AgentPhase::StreamingText | AgentPhase::ToolRunning => Color::Yellow,
        AgentPhase::WaitingApproval | AgentPhase::Failed => Color::Red,
    };
    let model_color = match app.model_phase {
        ModelPhase::Idle | ModelPhase::Completed => Color::DarkGray,
        ModelPhase::Streaming => Color::Cyan,
        ModelPhase::Failed => Color::Red,
    };
    let bold = Modifier::BOLD;
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(" 模式：", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    mode_label(app.mode),
                    Style::default().fg(mode_color).add_modifier(bold),
                ),
                Span::styled(
                    if app.mode == commands::AgentMode::Plan {
                        "  只读"
                    } else {
                        ""
                    },
                    Style::default().fg(mode_color).add_modifier(bold),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Agent：", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    agent_phase_label(app.agent_phase),
                    Style::default().fg(agent_color).add_modifier(bold),
                ),
                Span::styled("   模型：", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    model_phase_label(app.model_phase),
                    Style::default().fg(model_color).add_modifier(bold),
                ),
                Span::styled(
                    format!("   {}", app.status),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            context_line,
        ]),
        area,
    );
}

fn mode_label(mode: commands::AgentMode) -> &'static str {
    match mode {
        commands::AgentMode::Build => "构建",
        commands::AgentMode::Plan => "计划",
        commands::AgentMode::Explore => "探索",
    }
}

fn agent_phase_label(phase: AgentPhase) -> &'static str {
    match phase {
        AgentPhase::Idle => "空闲",
        AgentPhase::Thinking => "思考中",
        AgentPhase::StreamingText => "输出正文",
        AgentPhase::WaitingApproval => "等待确认",
        AgentPhase::ToolRunning => "执行工具",
        AgentPhase::Completed => "已完成",
        AgentPhase::Failed => "失败",
    }
}

fn model_phase_label(phase: ModelPhase) -> &'static str {
    match phase {
        ModelPhase::Idle => "空闲",
        ModelPhase::Streaming => "流式输出",
        ModelPhase::Completed => "已完成",
        ModelPhase::Failed => "失败",
    }
}

fn context_ring(percent: u64) -> &'static str {
    match percent {
        0 => "○",
        1..=12 => "◔",
        13..=25 => "◔",
        26..=37 => "◑",
        38..=50 => "◑",
        51..=62 => "◒",
        63..=75 => "◕",
        76..=87 => "◓",
        88..=99 => "◉",
        _ => "●",
    }
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (primary, sessions) = if app.settings.is_some() {
        (
            "Tab/↑/↓ 切换字段 | ←/→ 修改 | Enter 保存 | Esc 取消",
            "API Key 已隐藏，不会写入配置文件",
        )
    } else if app.pending_approval.is_some() {
        ("Y 批准 | N/Esc 拒绝 | Ctrl+C 退出", "")
    } else if app.busy {
        ("Esc 取消请求 | Ctrl+C 退出", "")
    } else {
        (
            "Enter 发送 | Shift+Enter 换行 | Ctrl+P 命令面板 | Ctrl+O 工具详情",
            "Ctrl+X 快捷键 | @ 文件 | ! Shell | / 命令 | Ctrl+C 退出",
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "快捷键  ",
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
    let popup = centered_rect(76, 18, area);
    let approval = app.pending_approval.as_ref().expect("approval exists");
    let mut lines = approval_lines(&approval.call, &approval.reason);
    let waited = approval.created_at.elapsed().as_secs();
    lines.push(Line::from(Span::styled(
        format!("已等待 {:02}:{:02}", waited / 60, waited % 60),
        Style::default().fg(Color::Yellow),
    )));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Y 批准    N 拒绝    Esc 拒绝",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" 需要确认 ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
        popup,
    );
}

fn approval_lines(call: &crate::provider::ToolCall, reason: &str) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("工具  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                tool_label(&call.name),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("风险  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                tool_risk(&call.name),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::default(),
    ];
    if let Some(arguments) = call.arguments.as_object() {
        for (key, value) in arguments.iter().take(7) {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<10}", argument_label(key)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(human_argument(key, value)),
            ]));
        }
    } else {
        lines.push(Line::from("没有结构化参数"));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("原因  ", Style::default().fg(Color::DarkGray)),
        Span::raw(fit_text(reason, 58)),
    ]));
    lines
}

fn tool_label(name: &str) -> String {
    match name {
        "file_write" => "Write file",
        "file_delete" => "Delete file",
        "file_move" => "Move file",
        "file_copy" => "Copy file",
        "file_mkdir" => "Create directory",
        "terminal_exec" => "Run program",
        "terminal_shell" => "Run shell command",
        "git" => "Run Git operation",
        "webfetch" => "Fetch web content",
        "agent_spawn" => "Start child agent",
        value if value.starts_with("browser_") => "Control external browser",
        value if value.starts_with("mcp:") => "Run external MCP tool",
        _ => name,
    }
    .to_owned()
}

fn tool_risk(name: &str) -> &'static str {
    match name {
        "file_delete" => "HIGH - removes workspace data",
        "terminal_shell" | "terminal_exec" | "git" => "HIGH - can change workspace state",
        "file_write" | "file_move" | "file_copy" | "file_mkdir" => {
            "MEDIUM - changes workspace files"
        }
        value if value.starts_with("browser_") || value.starts_with("mcp:") => {
            "MEDIUM - external side effect"
        }
        _ => "LOW - review parameters",
    }
}

fn argument_label(key: &str) -> &'static str {
    match key {
        "path" => "Path",
        "source" | "from" => "Source",
        "destination" | "to" => "Target",
        "command" => "命令",
        "args" => "Arguments",
        "cwd" => "Directory",
        "url" => "URL",
        "content" => "Content",
        "max_bytes" => "Max size",
        "timeout_seconds" => "Timeout",
        "prompt" => "任务",
        _ => "参数",
    }
}

fn human_argument(key: &str, value: &Value) -> String {
    let lower = key.to_ascii_lowercase();
    if lower.contains("secret")
        || lower.contains("token")
        || lower.contains("password")
        || lower.contains("api_key")
        || lower.contains("authorization")
    {
        return "[redacted]".into();
    }
    if key == "content" {
        return value
            .as_str()
            .map(|text| format!("{} bytes of text", text.len()))
            .unwrap_or_else(|| "structured content".into());
    }
    if key == "max_bytes" {
        return value
            .as_u64()
            .map(format_bytes)
            .unwrap_or_else(|| "default limit".into());
    }
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Array(values) => values
            .iter()
            .map(|item| item.as_str().unwrap_or("[value]"))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Bool(value) => {
            if *value {
                "yes".into()
            } else {
                "no".into()
            }
        }
        Value::Null => "未设置".into(),
        other => other.to_string(),
    };
    fit_text(&text, 58)
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

fn draw_settings(frame: &mut Frame<'_>, area: Rect, settings: &SettingsState) {
    let popup = centered_rect(84, 17, area);
    let key = if settings.api_key.is_empty() {
        if settings.has_existing_key {
            "********".to_owned()
        } else {
            "（未设置）".to_owned()
        }
    } else {
        "*".repeat(settings.api_key.chars().count().min(32))
    };
    let values = [
        (
            SettingsField::Preset,
            "提供商",
            settings.provider.preset.label().to_owned(),
        ),
        (
            SettingsField::Protocol,
            "协议",
            settings.provider.kind.label().to_owned(),
        ),
        (
            SettingsField::BaseUrl,
            "接口地址",
            settings.provider.base_url.clone(),
        ),
        (
            SettingsField::Model,
            "模型",
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
        "Tab/↑/↓：切换字段   ←/→：修改",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from("Enter：保存   Esc：取消"));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" 提供商设置 ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_palette(frame: &mut Frame<'_>, area: Rect, palette: &CommandPaletteState) {
    let popup = centered_rect(70, 14, area);
    let matches = commands::matches(&palette.query, 10);
    let mut lines = vec![Line::from(vec![
        Span::styled("/", Style::default().fg(Color::Cyan)),
        Span::raw(palette.query.clone()),
    ])];
    lines.push(Line::default());
    for (index, item) in matches.iter().enumerate() {
        let selected = index == palette.selected;
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{} {}",
                if selected { ">" } else { " " },
                commands::COMMAND_NAMES[item.index]
            ),
            style,
        )));
    }
    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "没有匹配的命令",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" 命令面板 ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_file_suggestions(frame: &mut Frame<'_>, input_area: Rect, app: &App) {
    let height = (app.file_suggestions.len() as u16 + 2).min(12);
    let popup = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width.min(64),
        height,
    };
    let items = app
        .file_suggestions
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let style = if index == app.file_selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(Span::styled(
                format!(
                    "{} @{value}",
                    if index == app.file_selected { ">" } else { " " }
                ),
                style,
            )))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(" 文件引用 ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
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

fn input_cursor_viewport(input: &str, cursor: usize, inner_width: usize) -> (String, usize, u16) {
    let cursor = cursor.min(input.len());
    let line_start = input[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_row = input[..cursor]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u16;
    let line = &input[line_start..];
    let (visible_line, column) =
        input_viewport(&line[..cursor.saturating_sub(line_start)], inner_width);
    let mut rendered = input[line_start..]
        .lines()
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    if rendered.is_empty() {
        rendered = visible_line.to_owned();
    }
    (rendered, column, line_row.min(2))
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

    #[test]
    fn diff_lines_receive_semantic_colors() {
        let lines = render_diff("@@ -1 +1 @@\n-old\n+new");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn approval_payload_is_human_readable_and_redacts_secrets() {
        let call = crate::provider::ToolCall {
            id: "call_1".into(),
            name: "terminal_shell".into(),
            arguments: serde_json::json!({
                "command": "git status",
                "api_key": "secret-value"
            }),
        };
        let text = approval_lines(&call, "needs approval")
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Run shell command"));
        assert!(text.contains("HIGH"));
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("secret-value"));
        assert!(!text.contains("\"command\""));
    }

    #[test]
    fn context_ring_tracks_usage_thresholds() {
        assert_eq!(context_ring(0), "○");
        assert_eq!(context_ring(50), "◑");
        assert_eq!(context_ring(90), "◉");
        assert_eq!(context_ring(100), "●");
    }
}
