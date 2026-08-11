use pulldown_cmark::{
    Alignment as MarkdownAlignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options,
    Parser, Tag, TagEnd,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use serde_json::Value;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{
        AgentPhase, App, CommandPaletteState, DisplayContent, DisplayKind, ModelPhase,
        SettingsField, SettingsState,
    },
    commands,
    output::{MessageLayout, OutputSelection, VisualLine},
};

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
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

fn draw_messages(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
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
    let viewport = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    let initial_layout = MessageLayout::new(lines, viewport, 0);
    let max_scroll = initial_layout.max_scroll();
    let scroll = app
        .output_scroll_top
        .unwrap_or_else(|| {
            if app.follow_output {
                max_scroll
            } else {
                max_scroll.saturating_sub(app.message_scroll)
            }
        })
        .min(max_scroll);
    let layout = MessageLayout::new(
        initial_layout
            .lines
            .iter()
            .map(|line| line.styled.clone())
            .collect(),
        viewport,
        scroll,
    );
    let selection = app.output_selection;
    let visible_lines = layout
        .visual_lines
        .iter()
        .skip(layout.scroll)
        .take(layout.viewport.height as usize)
        .map(|line| render_visual_line(&layout, line, selection))
        .collect::<Vec<_>>();
    app.message_layout = Some(layout);
    frame.render_widget(
        Paragraph::new(visible_lines)
            .block(Block::default().title(" 任务 ").borders(Borders::BOTTOM))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_visual_line(
    layout: &MessageLayout,
    visual: &VisualLine,
    selection: Option<OutputSelection>,
) -> Line<'static> {
    let Some(line) = layout.lines.get(visual.logical_line) else {
        return Line::default();
    };
    let local_start = visual.start.saturating_sub(line.start);
    let local_end = visual.end.saturating_sub(line.start);
    let selected_range = selection.and_then(OutputSelection::range);
    let mut spans = Vec::new();
    let mut span_offset = 0usize;
    for source in &line.styled.spans {
        let source_text = source.content.as_ref();
        for (offset, grapheme) in source_text.grapheme_indices(true) {
            let grapheme_start = span_offset + offset;
            let grapheme_end = grapheme_start + grapheme.len();
            if grapheme_end <= local_start || grapheme_start >= local_end {
                continue;
            }
            let global_start = line.start + grapheme_start;
            let global_end = line.start + grapheme_end;
            let is_selected =
                selected_range.is_some_and(|(start, end)| start < global_end && end > global_start);
            let style = if is_selected {
                source.style.fg(Color::Black).bg(Color::Cyan)
            } else {
                source.style
            };
            spans.push(Span::styled(grapheme.to_owned(), style));
        }
        span_offset += source_text.len();
    }
    Line::from(spans)
}

fn render_markdown(text: &str, base: Style) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM;
    MarkdownRenderer::new(base).render(Parser::new_ext(text, options).into_offset_iter(), text)
}

struct MarkdownRenderer {
    base: Style,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    styles: Vec<Style>,
    quote_depth: usize,
    lists: Vec<ListState>,
    assets: Vec<InlineAsset>,
    table: Option<TableState>,
    table_row: Option<TableRow>,
    table_cell: Option<Vec<Span<'static>>>,
    table_head: bool,
}

#[derive(Clone, Copy)]
enum ListState {
    Unordered,
    Ordered(u64),
}

enum InlineAsset {
    Link(String),
    Image(String),
}

struct TableState {
    alignments: Vec<MarkdownAlignment>,
    rows: Vec<TableRow>,
}

struct TableRow {
    header: bool,
    cells: Vec<Vec<Span<'static>>>,
}

impl MarkdownRenderer {
    fn new(base: Style) -> Self {
        Self {
            base,
            lines: Vec::new(),
            current: Vec::new(),
            styles: vec![base],
            quote_depth: 0,
            lists: Vec::new(),
            assets: Vec::new(),
            table: None,
            table_row: None,
            table_cell: None,
            table_head: false,
        }
    }

    fn render<'a, I>(mut self, events: I, source: &str) -> Vec<Line<'static>>
    where
        I: IntoIterator<Item = (Event<'a>, Range<usize>)>,
    {
        for (event, range) in events {
            self.event_spacing(source, range.start, &event);
            self.event(event);
        }
        self.flush_line(false);
        self.lines
    }

    fn event_spacing(&mut self, source: &str, offset: usize, event: &Event<'_>) {
        let is_block_start = matches!(
            event,
            Event::Start(
                Tag::Paragraph
                    | Tag::Heading { .. }
                    | Tag::BlockQuote(_)
                    | Tag::CodeBlock(_)
                    | Tag::List(_)
                    | Tag::FootnoteDefinition(_)
                    | Tag::Table(_)
                    | Tag::HtmlBlock
            )
        );
        if !is_block_start || self.lines.is_empty() || !self.current.is_empty() {
            return;
        }
        let trailing_newlines = source[..offset.min(source.len())]
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'\n')
            .count();
        for _ in 1..trailing_newlines {
            self.lines.push(Line::default());
        }
    }

    fn event<'a>(&mut self, event: Event<'a>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                let style = self.current_style();
                self.append_text(text.as_ref(), style);
            }
            Event::Code(text) => self.append_text(text.as_ref(), code_style()),
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.append_text(text.as_ref(), code_style());
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                self.append_text(text.as_ref(), raw_html_style());
            }
            Event::FootnoteReference(label) => {
                let style = self.current_style();
                self.append_text(&format!("[^{label}]"), style);
            }
            Event::SoftBreak | Event::HardBreak => self.flush_line(true),
            Event::Rule => {
                self.flush_line(false);
                self.lines.push(Line::from(Span::styled(
                    "────────",
                    self.base.fg(Color::DarkGray),
                )));
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.append_text(marker, self.current_style());
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.table_cell.is_none() {
                    self.ensure_prefix();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_line(false);
                let style = heading_style(self.base, level);
                self.push_style(style);
                self.ensure_prefix();
            }
            Tag::BlockQuote(kind) => {
                if self.table_cell.is_none() {
                    self.flush_line(false);
                    self.quote_depth = self.quote_depth.saturating_add(1);
                    let quote_style = self
                        .current_style()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC);
                    self.push_style(quote_style);
                    if let Some(kind) = kind {
                        self.ensure_quote_prefix();
                        self.append_span(Span::styled(
                            alert_label(kind),
                            Style::default()
                                .fg(alert_color(kind))
                                .add_modifier(Modifier::BOLD),
                        ));
                        self.flush_line(false);
                    }
                }
            }
            Tag::CodeBlock(kind) => {
                if self.table_cell.is_none() {
                    self.flush_line(false);
                    self.push_style(code_style());
                    self.ensure_prefix();
                    let label = match kind {
                        CodeBlockKind::Fenced(language) if !language.is_empty() => {
                            format!("[code: {language}]")
                        }
                        _ => "[code]".to_owned(),
                    };
                    self.current.push(Span::styled(label, code_fence_style()));
                    self.flush_line(false);
                }
            }
            Tag::List(start) => {
                if self.table_cell.is_none() {
                    self.lists.push(match start {
                        Some(number) => ListState::Ordered(number),
                        None => ListState::Unordered,
                    });
                }
            }
            Tag::Item => {
                if self.table_cell.is_none() {
                    self.flush_line(false);
                    self.ensure_quote_prefix();
                    let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                    let marker = match self.lists.last_mut() {
                        Some(ListState::Unordered) => "• ".to_owned(),
                        Some(ListState::Ordered(number)) => {
                            let marker = format!("{number}. ");
                            *number = number.saturating_add(1);
                            marker
                        }
                        None => String::new(),
                    };
                    self.current.push(Span::styled(
                        format!("{indent}{marker}"),
                        self.base.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ));
                }
            }
            Tag::FootnoteDefinition(label) => {
                self.flush_line(false);
                self.append_span(Span::styled(
                    format!("[^{label}]: "),
                    self.base.fg(Color::DarkGray),
                ));
            }
            Tag::Table(alignments) => {
                self.flush_line(false);
                self.table = Some(TableState {
                    alignments,
                    rows: Vec::new(),
                });
            }
            Tag::TableHead => {
                self.table_head = true;
                self.table_row = Some(TableRow {
                    header: true,
                    cells: Vec::new(),
                });
                self.push_style(self.current_style().add_modifier(Modifier::BOLD));
            }
            Tag::TableRow => {
                self.table_row = Some(TableRow {
                    header: self.table_head,
                    cells: Vec::new(),
                });
            }
            Tag::TableCell => {
                self.table_cell = Some(Vec::new());
            }
            Tag::Emphasis => self.push_style(self.current_style().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(self.current_style().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(self.current_style().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Superscript | Tag::Subscript => {
                self.push_style(self.current_style());
            }
            Tag::Link { dest_url, .. } => {
                self.assets.push(InlineAsset::Link(dest_url.to_string()));
                self.push_style(
                    self.current_style()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { dest_url, .. } => {
                self.assets.push(InlineAsset::Image(dest_url.to_string()));
                self.push_style(self.current_style().fg(Color::Cyan));
            }
            Tag::HtmlBlock => {
                self.flush_line(false);
                self.push_style(raw_html_style());
            }
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if self.table_cell.is_none() {
                    self.flush_line(true);
                }
            }
            TagEnd::Heading(_) => {
                if self.table_cell.is_none() {
                    self.flush_line(true);
                }
                self.pop_style();
            }
            TagEnd::BlockQuote(_) => {
                if self.table_cell.is_none() {
                    self.flush_line(false);
                    self.quote_depth = self.quote_depth.saturating_sub(1);
                    self.pop_style();
                }
            }
            TagEnd::CodeBlock => {
                if self.table_cell.is_none() {
                    self.flush_line(false);
                    self.current
                        .push(Span::styled("[end code]", code_fence_style()));
                    self.flush_line(false);
                    self.pop_style();
                }
            }
            TagEnd::List(_) => {
                if self.table_cell.is_none() {
                    self.flush_line(false);
                    self.lists.pop();
                }
            }
            TagEnd::Item => {
                if self.table_cell.is_none() {
                    self.flush_line(false);
                }
            }
            TagEnd::FootnoteDefinition => self.flush_line(false),
            TagEnd::TableHead => {
                self.table_head = false;
                if let Some(row) = self.table_row.take()
                    && let Some(table) = &mut self.table
                {
                    table.rows.push(row);
                }
                self.pop_style();
            }
            TagEnd::TableRow => {
                if let Some(row) = self.table_row.take()
                    && let Some(table) = &mut self.table
                {
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(cell) = self.table_cell.take()
                    && let Some(row) = &mut self.table_row
                {
                    row.cells.push(cell);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.lines.extend(render_table(table));
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => self.pop_style(),
            TagEnd::Link | TagEnd::Image => {
                self.pop_style();
                if let Some(asset) = self.assets.pop() {
                    let destination = match asset {
                        InlineAsset::Link(destination) | InlineAsset::Image(destination) => {
                            destination
                        }
                    };
                    self.append_span(Span::styled(
                        format!(" ({destination})"),
                        self.base.fg(Color::DarkGray),
                    ));
                }
            }
            TagEnd::HtmlBlock => {
                self.flush_line(false);
                self.pop_style();
            }
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn current_style(&self) -> Style {
        self.styles.last().copied().unwrap_or(self.base)
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(style);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn append_text(&mut self, text: &str, style: Style) {
        if self.table_cell.is_some() {
            let text = text.replace(['\r', '\n'], " ");
            if !text.is_empty() {
                self.append_span(Span::styled(text, style));
            }
            return;
        }
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush_line(true);
            }
            if !part.is_empty() {
                self.ensure_prefix();
                self.append_span(Span::styled(part.to_owned(), style));
            }
        }
    }

    fn append_span(&mut self, span: Span<'static>) {
        if let Some(cell) = &mut self.table_cell {
            cell.push(span);
        } else {
            self.current.push(span);
        }
    }

    fn ensure_quote_prefix(&mut self) {
        if self.current.is_empty() && self.quote_depth > 0 {
            self.current.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                self.base.fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ));
        }
    }

    fn ensure_prefix(&mut self) {
        if self.table_cell.is_some() || !self.current.is_empty() {
            return;
        }
        self.ensure_quote_prefix();
        if self.current.is_empty() && !self.lists.is_empty() {
            self.current.push(Span::raw("  ".repeat(self.lists.len())));
        }
    }

    fn flush_line(&mut self, force: bool) {
        if !self.current.is_empty() || force {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current)));
        }
    }
}

fn render_table(table: TableState) -> Vec<Line<'static>> {
    const MAX_TABLE_CELL_WIDTH: usize = 24;
    let alignments = table.alignments;
    let column_count = table
        .rows
        .iter()
        .map(|row| row.cells.len())
        .max()
        .unwrap_or(0);
    if column_count == 0 {
        return Vec::new();
    }
    let mut widths = vec![3; column_count];
    for row in &table.rows {
        for (column, cell) in row.cells.iter().enumerate() {
            let width = cell
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            widths[column] = widths[column].max(width.min(MAX_TABLE_CELL_WIDTH));
        }
    }

    let mut lines = Vec::new();
    let mut rendered_header = false;
    for row in table.rows {
        let mut spans = vec![Span::styled("| ", table_border_style())];
        for (column, width) in widths.iter().enumerate() {
            let cell = row.cells.get(column).cloned().unwrap_or_default();
            let alignment = alignments
                .get(column)
                .copied()
                .unwrap_or(MarkdownAlignment::None);
            spans.extend(render_table_cell(cell, *width, alignment));
            spans.push(Span::styled(" | ", table_border_style()));
        }
        lines.push(Line::from(spans));
        if row.header && !rendered_header {
            rendered_header = true;
            let mut separator = vec![Span::styled("| ", table_border_style())];
            for (column, width) in widths.iter().enumerate() {
                let alignment = alignments
                    .get(column)
                    .copied()
                    .unwrap_or(MarkdownAlignment::None);
                separator.push(Span::styled(
                    table_separator(*width, alignment),
                    table_border_style(),
                ));
                separator.push(Span::styled(" | ", table_border_style()));
            }
            lines.push(Line::from(separator));
        }
    }
    lines
}

fn render_table_cell(
    cell: Vec<Span<'static>>,
    target_width: usize,
    alignment: MarkdownAlignment,
) -> Vec<Span<'static>> {
    let cell_width = cell
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let extra = target_width.saturating_sub(cell_width);
    let (left, right) = match alignment {
        MarkdownAlignment::Right => (extra, 0),
        MarkdownAlignment::Center => {
            let left = extra / 2;
            (left, extra.saturating_sub(left))
        }
        MarkdownAlignment::Left | MarkdownAlignment::None => (0, extra),
    };
    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::styled(" ".repeat(left), table_border_style()));
    }
    spans.extend(cell);
    if right > 0 {
        spans.push(Span::styled(" ".repeat(right), table_border_style()));
    }
    spans
}

fn table_separator(width: usize, alignment: MarkdownAlignment) -> String {
    let dashes = "-".repeat(width);
    match alignment {
        MarkdownAlignment::Left => format!(":{dashes}"),
        MarkdownAlignment::Center => format!(":{dashes}:"),
        MarkdownAlignment::Right => format!("{dashes}:"),
        MarkdownAlignment::None => dashes,
    }
}

fn heading_style(base: Style, level: HeadingLevel) -> Style {
    base.fg(if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
        Color::Cyan
    } else {
        Color::Blue
    })
    .add_modifier(Modifier::BOLD)
}

fn alert_label(kind: BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "NOTE",
        BlockQuoteKind::Tip => "TIP",
        BlockQuoteKind::Important => "IMPORTANT",
        BlockQuoteKind::Warning => "WARNING",
        BlockQuoteKind::Caution => "CAUTION",
    }
}

fn alert_color(kind: BlockQuoteKind) -> Color {
    match kind {
        BlockQuoteKind::Note => Color::Cyan,
        BlockQuoteKind::Tip => Color::Green,
        BlockQuoteKind::Important => Color::Magenta,
        BlockQuoteKind::Warning => Color::Yellow,
        BlockQuoteKind::Caution => Color::Red,
    }
}

fn raw_html_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn table_border_style() -> Style {
    Style::default().fg(Color::DarkGray)
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

    fn markdown_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn markdown_renders_nested_lists_tasks_tables_and_footnotes() {
        let lines = render_markdown(
            "> outer quote\n>\n> > nested **text**\n\n1. first\n   - nested\n   - [x] done\n\n| 名称 | 值 |\n| :--- | ---: |\n| 中文 | 🙂 |\n\n引用[^1]\n\n[^1]: 脚注内容",
            Style::default(),
        );
        let text = markdown_text(&lines);
        assert!(text.contains("│ outer quote"));
        assert!(text.contains("│ │ nested text"));
        assert!(text.contains("1. first"));
        assert!(text.contains("  • nested"));
        assert!(text.contains("  • [x] done"));
        assert!(text.contains("| 名称"));
        assert!(text.contains("| ---"));
        assert!(text.contains("| 中文"));
        assert!(text.contains("引用[^1]"));
        assert!(text.contains("[^1]: 脚注内容"));
    }

    #[test]
    fn markdown_renders_gfm_alert_labels_and_nested_quotes() {
        let lines = render_markdown(
            "> [!NOTE]\n> note body\n\n> [!TIP]\n> tip body\n\n> [!IMPORTANT]\n> important body\n\n> [!WARNING]\n> warning body\n\n> [!CAUTION]\n> caution body\n\n> ordinary\n> > [!NOTE]\n> > nested note\n\nafter",
            Style::default(),
        );
        let text = markdown_text(&lines);
        for label in ["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION"] {
            assert!(
                text.contains(&format!("│ {label}")),
                "missing {label} in {text}"
            );
        }
        assert!(text.contains("│ note body"));
        assert!(text.contains("│ │ NOTE"));
        assert!(text.contains("│ │ nested note"));
        assert!(text.contains("after"));
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content == "WARNING" && span.style.fg == Some(Color::Yellow))
        }));
    }

    #[test]
    fn markdown_table_uses_alignment_padding_and_visible_delimiters() {
        let lines = render_markdown(
            "| Left | Center | Right |\n| :--- | :---: | ---: |\n| a | b | c |",
            Style::default(),
        );
        let text = markdown_text(&lines);
        assert!(text.contains("| :---- | :------: | -----: |"));
        assert!(text.contains("| a    |   b    |     c |"));
    }

    #[test]
    fn markdown_renders_links_images_nested_styles_and_html_as_text() {
        let lines = render_markdown(
            r#"**粗 *体*** ~~删~~ `代码` [链接](https://example.test/a) ![图片](https://example.test/i.png) <span>HTML</span> \*字面星号\* 中文🙂"#,
            Style::default(),
        );
        let text = markdown_text(&lines);
        assert!(text.contains("粗 体"));
        assert!(text.contains("删"));
        assert!(text.contains("代码"));
        assert!(text.contains("链接 (https://example.test/a)"));
        assert!(text.contains("图片 (https://example.test/i.png)"));
        assert!(text.contains("<span>HTML</span>"));
        assert!(text.contains("*字面星号* 中文🙂"));
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.style.add_modifier(Modifier::BOLD) == span.style)
        }));
    }

    #[test]
    fn markdown_handles_code_variants_soft_breaks_and_incomplete_streams() {
        let lines = render_markdown(
            "before\nsoft break\n\n    indented code\n\n~~~text\nfenced\n~~~\n\n**incomplete",
            Style::default(),
        );
        let text = markdown_text(&lines);
        assert!(text.contains("before"));
        assert!(text.contains("soft break"));
        assert!(text.contains("indented code"));
        assert!(text.contains("[code: text]"));
        assert!(text.contains("fenced"));
        assert!(text.contains("[end code]"));
        assert!(text.contains("incomplete"));

        let hard_break = markdown_text(&render_markdown("one  \ntwo", Style::default()));
        assert_eq!(hard_break, "one\ntwo");
        let paragraph_gap = markdown_text(&render_markdown("first\n\nsecond", Style::default()));
        assert_eq!(paragraph_gap, "first\n\nsecond");
    }

    #[test]
    fn markdown_lines_feed_message_layout_without_losing_copy_text() {
        let lines = render_markdown(
            r#"| A | B |
| --- | --- |
| **中文** | *🙂* |
| 长文本 | [链接](https://example.test) |
| 组合 é | Emoji 👩‍💻 |"#,
            Style::default(),
        );
        let layout = MessageLayout::new(lines, Rect::new(0, 0, 12, 10), 0);
        let selection = OutputSelection {
            anchor: 0,
            active: layout.text.len(),
            dragging: false,
        };
        assert_eq!(layout.selected_text(selection), Some(layout.text.as_str()));
        assert!(layout.text.contains("中文"));
        assert!(layout.text.contains("🙂"));
        assert!(layout.text.contains("长文本"));
        assert!(layout.text.contains("https://example.test"));
        assert!(layout.text.contains("e\u{301}"));
        assert!(layout.text.contains("👩‍💻"));
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
