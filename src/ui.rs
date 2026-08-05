use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::app::{App, DisplayKind};

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let horizontal = if area.width >= 90 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(22), Constraint::Min(40)])
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
        ])
        .split(horizontal[1]);
    draw_messages(frame, main[0], app);
    draw_input(frame, main[1], app);
    draw_status(frame, main[2], app);
    if app.pending_approval.is_some() {
        draw_approval(frame, area, app);
    }
}

fn draw_sessions(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let workspace = app
        .workspace
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| app.workspace.to_string_lossy());
    let items = vec![
        ListItem::new(Line::from(Span::styled(
            workspace,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))),
        ListItem::new(Line::from(format!("{}...", &app.session_id[..8]))),
    ];
    frame.render_widget(
        List::new(items).block(Block::default().title(" Sessions ").borders(Borders::RIGHT)),
        area,
    );
}

fn draw_messages(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    for entry in &app.entries {
        let (label, color) = match entry.kind {
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
        if entry.text.is_empty() {
            lines.push(Line::from(Span::styled(
                "...",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.extend(entry.text.lines().map(|line| Line::from(line.to_owned())));
        }
        lines.push(Line::default());
    }
    let visible_height = area.height.saturating_sub(2) as usize;
    let scroll = lines
        .len()
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

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let style = if app.busy {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };
    frame.render_widget(
        Paragraph::new(app.input.as_str())
            .style(style)
            .block(Block::default().title(" Input ").borders(Borders::ALL)),
        area,
    );
    if !app.busy {
        let cursor_x = area.x
            + 1
            + app
                .input
                .chars()
                .count()
                .min(area.width.saturating_sub(3) as usize) as u16;
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
