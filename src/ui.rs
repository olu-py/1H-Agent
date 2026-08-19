use pulldown_cmark::{
    Alignment as MarkdownAlignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options,
    Parser, Tag, TagEnd,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use serde_json::Value;
use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    ops::Range,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    agent::ChildSessionStatus,
    app::{
        App, CommandPaletteState, DisplayContent, DisplayKind, ThinkingDisplay, TodoDisplay,
        TodoStatus, ToolDisplay, ToolDisplayStatus,
    },
    commands::{self, AgentMode},
    input::input_cursor_viewport,
    output::{InteractionTarget, MessageLayout, OutputSelection, VisualLine},
    secrets,
    settings::{FIELDS, SettingsField, SettingsForm, SettingsState},
    storage::SessionSummary,
    ui_layout::{Density, HeightClass, compute_layout, message_block},
    ui_theme::{UiTheme, VisualRole},
    ui_view_model::{
        FooterLine, InputView, ThinkingControlView, UiSegment, UiViewModel, mode_label,
    },
};

#[cfg(test)]
use crate::input::input_viewport;

struct RenderedMessageLines {
    lines: Vec<Line<'static>>,
    interactions: Vec<Option<InteractionTarget>>,
    thinking_before: Option<usize>,
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let density = Density::from_width(area.width);
    let height = HeightClass::from_height(area.height);
    let layout = compute_layout(area, density, height);
    let view = UiViewModel::from_app(app, density, height, layout.footer.width as usize);
    #[cfg(test)]
    {
        app.current.footer_rebuild_count += 1;
    }
    let theme = UiTheme::default();

    if let Some(sessions) = layout.sessions {
        draw_sessions(frame, sessions, app, &theme);
    } else {
        app.session_panel_rect = None;
    }
    draw_messages(
        frame,
        layout.messages_outer,
        layout.messages_inner,
        app,
        &theme,
    );
    draw_input(frame, layout.input, app, &view.input, &theme);
    if !app.file_suggestions.is_empty() && app.palette.is_none() && app.settings.is_none() {
        draw_file_suggestions(frame, layout.input, app);
    }
    draw_footer(frame, layout.footer, &view, app, &theme);
    if app.thinking_menu_open {
        draw_thinking_menu(frame, area, layout.footer, app, &view.thinking, &theme);
    } else {
        app.thinking_menu_rect = None;
    }
    if app.provider_menu_open {
        draw_provider_menu(frame, area, layout.footer, app, &theme);
    } else {
        app.provider_menu_rect = None;
    }
    if app.model_menu_open {
        draw_model_menu(frame, area, layout.footer, app, &theme);
    } else {
        app.model_menu_rect = None;
    }
    app.settings_rect = app.settings.as_ref().map(|settings| match settings {
        SettingsState::List(_) => centered_rect(78, 20, area),
        SettingsState::Templates(_) => centered_rect(68, 18, area),
        SettingsState::Form(_) => centered_rect(88, 22, area),
    });
    if let Some(settings) = &app.settings {
        draw_settings(frame, area, settings, &theme);
    }
    if let Some(palette) = &app.palette {
        draw_palette(frame, area, palette, &theme);
    }
    if app.has_pending_approval() {
        draw_approval(frame, area, app, &theme);
    }
}

const SESSIONS_TITLE_OFFSET: u16 = 1;
const SESSIONS_HEADER_ROWS: u16 = 4;
const SESSIONS_TRAILING_ROWS: u16 = 1;

/// Number of session rows visible below the panel title and the four-row
/// header. The `List` block has a title, so its inner area starts one row lower
/// than the panel rect.
fn session_visible_slots(area_height: u16) -> usize {
    area_height
        .saturating_sub(SESSIONS_TITLE_OFFSET + SESSIONS_HEADER_ROWS + SESSIONS_TRAILING_ROWS)
        as usize
}

/// A flattened, depth-annotated session for tree rendering and hit-testing.
pub(crate) struct SessionRow {
    pub id: String,
    pub title: String,
    pub depth: usize,
    pub has_children: bool,
    pub expanded: bool,
}

/// Flattens the session list into depth-first tree order, dropping the
/// children of collapsed parents. Roots (`parent_id == None`) come first, each
/// followed by its visible descendants. Parents are collapsed by default and
/// only expand when the user clicks them; the active session does not force an
/// expansion.
pub(crate) fn flatten_session_tree(
    sessions: &[SessionSummary],
    expanded: &HashSet<String>,
) -> Vec<SessionRow> {
    let mut rows = Vec::new();
    for root in sessions
        .iter()
        .filter(|session| session.parent_id.is_none())
    {
        push_session_tree(root, 0, sessions, expanded, &mut rows);
    }
    rows
}

fn push_session_tree(
    session: &SessionSummary,
    depth: usize,
    all: &[SessionSummary],
    expanded: &HashSet<String>,
    rows: &mut Vec<SessionRow>,
) {
    let has_children = all
        .iter()
        .any(|child| child.parent_id.as_deref() == Some(session.id.as_str()));
    let is_expanded = has_children && expanded.contains(&session.id);
    rows.push(SessionRow {
        id: session.id.clone(),
        title: session.title.clone(),
        depth,
        has_children,
        expanded: is_expanded,
    });
    if is_expanded {
        for child in all
            .iter()
            .filter(|child| child.parent_id.as_deref() == Some(session.id.as_str()))
        {
            push_session_tree(child, depth + 1, all, expanded, rows);
        }
    }
}

/// Window start such that the current session stays visible, pinned to the
/// bottom slot whenever the list has scrolled. Shared by rendering and hit
/// testing so a click always maps to the same session that was drawn.
fn session_window_start(total: usize, current: usize, visible_slots: usize) -> usize {
    current
        .saturating_add(1)
        .saturating_sub(visible_slots)
        .min(total.saturating_sub(visible_slots))
}

/// Maps a mouse position inside the sessions panel to the session list index,
/// or `None` when the position is outside the panel, in the header, or in the
/// trailing blank rows.
pub(crate) fn session_index_at(
    area: Rect,
    column: u16,
    row: u16,
    total: usize,
    current: usize,
) -> Option<usize> {
    if area.width < 2 {
        return None;
    }
    let list_top = area.y.saturating_add(SESSIONS_TITLE_OFFSET);
    let list_right = area.right().saturating_sub(1);
    if column < area.x || column >= list_right || row < list_top || row >= area.bottom() {
        return None;
    }
    let header_end = list_top.saturating_add(SESSIONS_HEADER_ROWS);
    if row < header_end {
        return None;
    }
    let visible_slots = session_visible_slots(area.height);
    if visible_slots == 0 {
        return None;
    }
    let offset = row.saturating_sub(header_end) as usize;
    if offset >= visible_slots {
        return None;
    }
    let start = session_window_start(total, current, visible_slots);
    let index = start.saturating_add(offset);
    (index < total).then_some(index)
}

fn draw_sessions(frame: &mut Frame<'_>, area: Rect, app: &mut App, theme: &UiTheme) {
    app.session_panel_rect = Some(area);
    let workspace = app
        .workspace
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| app.workspace.to_string_lossy());
    let content_width = area.width.saturating_sub(3) as usize;
    let mut items = vec![
        ListItem::new(Line::from(Span::styled(
            "Alt+Up/Down  切换会话",
            theme.style(VisualRole::Muted),
        ))),
        ListItem::new(Line::from(Span::styled(
            "Ctrl+N       新建会话",
            theme.style(VisualRole::Muted),
        ))),
        ListItem::new(Line::default()),
        ListItem::new(Line::from(Span::styled(
            fit_text(&workspace, content_width),
            theme.strong(VisualRole::Accent),
        ))),
    ];
    let visible_sessions = session_visible_slots(area.height);
    let rows = flatten_session_tree(&app.sessions, &app.expanded_sessions);
    let current = rows
        .iter()
        .position(|row| row.id == app.current.session_id)
        .unwrap_or(0);
    let start = session_window_start(rows.len(), current, visible_sessions);
    for row in rows.iter().skip(start).take(visible_sessions) {
        let active = row.id == app.current.session_id;
        let arrow = if row.has_children {
            if row.expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        };
        let indent = "  ".repeat(row.depth);
        let marker = if active { ">" } else { " " };
        let child_progress = app.child_status.get(&row.id);
        let waiting_approval = app.session_waiting_approval(&row.id);
        let status_text = match (child_progress, waiting_approval) {
            (_, true) => " ⏳审批".to_owned(),
            (Some(progress), false) if progress.status == ChildSessionStatus::Completed => {
                String::new()
            }
            (Some(progress), false) => format!(" ·{}", progress.label()),
            (None, false) => String::new(),
        };
        let style = if active {
            theme.selected
        } else if row.has_children {
            theme.strong(VisualRole::Accent)
        } else {
            theme.style(VisualRole::Primary)
        };
        let title = fit_text(
            &row.title,
            content_width.saturating_sub(
                row.depth
                    .saturating_mul(2)
                    .saturating_add(4)
                    .saturating_add(
                        status_text
                            .chars()
                            .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
                            .sum::<usize>(),
                    ),
            ),
        );
        items.push(ListItem::new(Line::from(Span::styled(
            format!("{indent}{arrow}{marker} {title}{status_text}"),
            style,
        ))));
    }
    frame.render_widget(
        List::new(items).block(Block::default().title(" 会话 ").borders(Borders::RIGHT)),
        area,
    );
}

fn draw_messages(
    frame: &mut Frame<'_>,
    area: Rect,
    viewport: Rect,
    app: &mut App,
    theme: &UiTheme,
) {
    let block = message_block();
    update_message_layout(app, viewport);
    let Some(layout) = &app.current.message_layout else {
        frame.render_widget(
            Paragraph::new(Vec::<Line<'static>>::new()).block(block),
            area,
        );
        return;
    };
    let selection = app.current.output_selection;
    let visible_lines = layout
        .visual_lines
        .iter()
        .skip(layout.scroll)
        .take(layout.viewport.height as usize)
        .map(|line| render_visual_line(layout, line, selection, theme))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible_lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
    draw_todo_window(frame, viewport, app, theme);
}

pub(crate) const TODO_WINDOW_MAX_WIDTH: u16 = 44;

pub(crate) fn todo_window_rect(viewport: Rect, task_count: usize) -> Option<Rect> {
    let minimum_height = if task_count == 1 { 3 } else { 4 };
    if task_count == 0 || viewport.width < 4 || viewport.height < minimum_height {
        return None;
    }
    let width = TODO_WINDOW_MAX_WIDTH.min(viewport.width);
    let desired_height = task_count.saturating_add(2).min(usize::from(u16::MAX));
    let height = desired_height.min(usize::from(viewport.height)) as u16;
    if height < minimum_height {
        return None;
    }
    Some(Rect {
        x: viewport.right().saturating_sub(width),
        y: viewport.bottom().saturating_sub(height),
        width,
        height,
    })
}

fn draw_todo_window(frame: &mut Frame<'_>, viewport: Rect, app: &mut App, theme: &UiTheme) {
    if app.current.todos.is_empty() {
        app.todo_window_rect = None;
        return;
    }
    let Some(rect) = todo_window_rect(viewport, app.current.todos.len()) else {
        app.todo_window_rect = None;
        return;
    };
    app.todo_window_rect = Some(rect);
    let todo = TodoDisplay {
        tasks: app.current.todos.clone(),
    };
    let lines = todo_window_lines(
        &todo,
        usize::from(rect.width.saturating_sub(2)),
        usize::from(rect.height.saturating_sub(2)),
    );
    let (done, total) = todo.progress();
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(format!(" 任务清单 {done}/{total} "))
                .borders(Borders::ALL)
                .border_style(theme.focus_border),
        ),
        rect,
    );
}

pub(crate) fn todo_visible_task_count(task_count: usize, visible_rows: usize) -> usize {
    if task_count > visible_rows {
        visible_rows.saturating_sub(1)
    } else {
        task_count
    }
}

fn todo_window_lines(todo: &TodoDisplay, width: usize, visible_rows: usize) -> Vec<Line<'static>> {
    let visible_tasks = todo_visible_task_count(todo.tasks.len(), visible_rows);
    let tasks = &todo.tasks[..visible_tasks];
    let mut lines = Vec::new();
    if todo.tasks.len() > visible_rows {
        let hidden = todo.tasks.len().saturating_sub(visible_tasks);
        if hidden > 0 {
            lines.push(Line::from(Span::styled(
                format!("… 还有 {hidden} 项"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    for (index, task) in tasks.iter().enumerate() {
        let number = (index + 1).to_string();
        let prefix_width = number.width() + 4;
        let title = fit_text(&task.title, width.saturating_sub(prefix_width));
        let (marker, color) = match task.status {
            TodoStatus::Pending => ("○", Color::DarkGray),
            TodoStatus::InProgress => ("◐", Color::Yellow),
            TodoStatus::Done => ("●", Color::Green),
        };
        lines.push(Line::from(vec![
            Span::styled(marker.to_owned(), Style::default().fg(color)),
            Span::raw(format!(" {number}. ")),
            Span::raw(title),
        ]));
    }
    lines
}

pub(crate) fn update_message_layout(app: &mut App, viewport: Rect) {
    let cached_width = app
        .current
        .message_layout
        .as_ref()
        .map(|layout| layout.width);
    let viewport_width = viewport.width.max(1) as usize;
    if app.current.output_layout_dirty || app.current.message_layout.is_none() {
        drop(app.current.message_layout.take());
        let rendered = render_message_lines(app, viewport_width);
        app.current.message_layout = Some(MessageLayout::new_with_interactions(
            rendered.lines,
            rendered.interactions,
            viewport,
            0,
            rendered.thinking_before,
        ));
        #[cfg(test)]
        {
            app.current.output_layout_rebuild_count += 1;
        }
    } else if cached_width != Some(viewport_width) {
        let layout = app
            .current
            .message_layout
            .take()
            .expect("layout existence was checked above");
        app.current.message_layout = Some(layout.reflow(viewport));
        #[cfg(test)]
        {
            app.current.output_layout_rebuild_count += 1;
        }
    } else if let Some(layout) = &mut app.current.message_layout {
        layout.update_viewport(viewport);
    }

    let existing_live_rows = app
        .current
        .message_layout
        .as_ref()
        .map_or(0, |layout| layout.live_thinking_lines.len());
    let thinking_update =
        live_thinking_lines(app, viewport.width.max(1) as usize, existing_live_rows);
    if let Some(layout) = &mut app.current.message_layout {
        if let Some(lines) = thinking_update.lines {
            layout.set_live_thinking_lines(lines);
        } else {
            layout.set_live_thinking_title(thinking_update.title);
        }
        let max_scroll = layout.max_scroll();
        let anchored_scroll =
            app.layout_restore_anchor
                .take()
                .and_then(|(target, relative_row)| {
                    layout
                        .visual_lines
                        .iter()
                        .position(|line| line.interaction.as_ref() == Some(&target))
                        .map(|visual_row| visual_row.saturating_sub(relative_row))
                });
        let scroll = anchored_scroll
            .or(app.current.output_scroll_top)
            .unwrap_or_else(|| {
                if app.current.follow_output {
                    max_scroll
                } else {
                    max_scroll.saturating_sub(app.current.message_scroll)
                }
            })
            .min(max_scroll);
        if anchored_scroll.is_some() {
            app.current.output_scroll_top = Some(scroll);
            app.current.message_scroll = max_scroll.saturating_sub(scroll);
        }
        layout.set_scroll(scroll);
    }
    app.current.output_layout_dirty = false;
}

fn render_message_lines(app: &mut App, width: usize) -> RenderedMessageLines {
    let theme = UiTheme::default();
    let mut lines = Vec::new();
    let mut interactions = Vec::new();
    let mut thinking_before = None;
    let mut in_tool_group = false;
    let mut parsed_markdown = 0usize;
    let current = &mut app.current;
    let entries = &current.entries;
    let expanded_tools = &current.expanded_tools;
    let expanded_thinking = &current.expanded_thinking;
    let thinking_anchor = current.thinking_anchor;
    let render_cache = &mut current.markdown_render_cache;
    for (entry_index, entry) in entries.iter().enumerate() {
        if thinking_anchor == Some(entry_index) {
            thinking_before = Some(lines.len());
            in_tool_group = false;
        }
        if let DisplayContent::Tool(tool) = &entry.content {
            if !in_tool_group {
                push_rendered_line(
                    &mut lines,
                    &mut interactions,
                    Line::from(Span::styled("工具", theme.strong(VisualRole::Tool))),
                    None,
                );
                in_tool_group = true;
            }
            render_tool(
                tool,
                expanded_tools.contains(&tool.call_id),
                width,
                &mut lines,
                &mut interactions,
            );
            let group_ends = entries.get(entry_index + 1).is_none_or(|next| {
                !matches!(next.content, DisplayContent::Tool(_))
                    || thinking_anchor == Some(entry_index + 1)
            });
            if group_ends {
                push_rendered_line(&mut lines, &mut interactions, Line::default(), None);
                in_tool_group = false;
            }
            continue;
        }
        in_tool_group = false;
        if let DisplayContent::Thinking(thinking) = &entry.content {
            let expanded = expanded_thinking.contains(&thinking.id);
            let expanded_body = expanded.then(|| {
                let (rendered, parsed) = cached_markdown(
                    render_cache,
                    entry_index,
                    &thinking.content,
                    theme.style(VisualRole::Primary),
                    1,
                );
                parsed_markdown += usize::from(parsed);
                rendered
            });
            render_thinking_summary(
                thinking,
                expanded,
                expanded_body.as_deref(),
                width,
                &mut lines,
                &mut interactions,
            );
            push_rendered_line(&mut lines, &mut interactions, Line::default(), None);
            continue;
        }
        if let DisplayContent::Markdown(text) = &entry.content
            && matches!(entry.kind, DisplayKind::System | DisplayKind::Error)
        {
            let (prefix, role) = if matches!(entry.kind, DisplayKind::Error) {
                ("× ", VisualRole::Danger)
            } else {
                ("", VisualRole::Muted)
            };
            push_rendered_line(
                &mut lines,
                &mut interactions,
                Line::from(Span::styled(
                    format!("{prefix}{}", text.replace('\n', " ")),
                    theme.style(role),
                )),
                None,
            );
            push_rendered_line(&mut lines, &mut interactions, Line::default(), None);
            continue;
        }
        let (label, role) = match &entry.kind {
            DisplayKind::User => ("用户", VisualRole::User),
            DisplayKind::Assistant => ("Agent", VisualRole::Accent),
            DisplayKind::Thinking => ("思考摘要", VisualRole::Thinking),
            DisplayKind::Tool => ("工具", VisualRole::Tool),
            DisplayKind::Error => ("错误", VisualRole::Danger),
            DisplayKind::System => ("系统", VisualRole::Muted),
        };
        push_rendered_line(
            &mut lines,
            &mut interactions,
            Line::from(Span::styled(label, theme.strong(role))),
            None,
        );
        let content_style = theme.style(VisualRole::Primary);
        match &entry.content {
            DisplayContent::Markdown(text) => {
                if text.is_empty() {
                    push_rendered_line(
                        &mut lines,
                        &mut interactions,
                        Line::from(Span::styled("...", theme.style(VisualRole::Muted))),
                        None,
                    );
                } else {
                    let (rendered, parsed) =
                        cached_markdown(render_cache, entry_index, text, content_style, 0);
                    parsed_markdown += usize::from(parsed);
                    for line in rendered {
                        push_rendered_line(&mut lines, &mut interactions, line, None);
                    }
                }
            }
            DisplayContent::Diff(diff) => {
                for line in render_diff(diff) {
                    push_rendered_line(&mut lines, &mut interactions, line, None);
                }
            }
            DisplayContent::Tool(_) => unreachable!("tool entries are rendered as a group"),
            DisplayContent::Thinking(_) => unreachable!("thinking entries are rendered inline"),
        }
        push_rendered_line(&mut lines, &mut interactions, Line::default(), None);
    }
    if thinking_anchor.is_some() && thinking_before.is_none() {
        thinking_before = Some(lines.len());
    }
    #[cfg(test)]
    {
        current.markdown_parse_count += parsed_markdown;
    }
    RenderedMessageLines {
        lines,
        interactions,
        thinking_before,
    }
}

fn cached_markdown(
    cache: &mut std::collections::HashMap<usize, crate::output::CachedMarkdown>,
    entry_index: usize,
    text: &str,
    base: Style,
    variant: u8,
) -> (Vec<Line<'static>>, bool) {
    let mut hasher = DefaultHasher::new();
    variant.hash(&mut hasher);
    text.hash(&mut hasher);
    let fingerprint = hasher.finish();
    if let Some(cached) = cache.get(&entry_index)
        && cached.fingerprint == fingerprint
    {
        return (cached.lines.clone(), false);
    }
    let lines = render_markdown(text, base);
    cache.insert(
        entry_index,
        crate::output::CachedMarkdown {
            fingerprint,
            lines: lines.clone(),
        },
    );
    (lines, true)
}

fn push_rendered_line(
    lines: &mut Vec<Line<'static>>,
    interactions: &mut Vec<Option<InteractionTarget>>,
    line: Line<'static>,
    interaction: Option<InteractionTarget>,
) {
    lines.push(line);
    interactions.push(interaction);
}

fn render_visual_line(
    layout: &MessageLayout,
    visual: &VisualLine,
    selection: Option<OutputSelection>,
    theme: &UiTheme,
) -> Line<'static> {
    if visual.synthetic {
        return Line::from(Span::styled(
            layout
                .live_thinking_lines
                .get(visual.start)
                .cloned()
                .unwrap_or_default(),
            theme.style(VisualRole::Thinking),
        ));
    }
    let Some(line) = layout.lines.get(visual.logical_line) else {
        return Line::default();
    };
    let local_start = visual.start.saturating_sub(line.start);
    let local_end = visual.end.saturating_sub(line.start);
    let selected_range = selection.and_then(OutputSelection::range);
    let first_run = line
        .style_runs
        .partition_point(|run| run.end <= local_start);
    let mut parts = Vec::<(String, Style)>::new();
    for run in line.style_runs.iter().skip(first_run) {
        if run.start >= local_end {
            break;
        }
        let start = run.start.max(local_start);
        let end = run.end.min(local_end);
        if start >= end {
            continue;
        }
        let global_start = line.start + start;
        let global_end = line.start + end;
        if let Some((selected_start, selected_end)) = selected_range
            && selected_start < global_end
            && selected_end > global_start
        {
            let before_end = selected_start.clamp(global_start, global_end);
            push_styled_slice(
                &mut parts,
                &line.text[start..before_end - line.start],
                run.style,
            );
            let highlighted_start = selected_start.max(global_start);
            let highlighted_end = selected_end.min(global_end);
            push_styled_slice(
                &mut parts,
                &line.text[highlighted_start - line.start..highlighted_end - line.start],
                run.style.fg(Color::Black).bg(Color::Cyan),
            );
            push_styled_slice(
                &mut parts,
                &line.text[highlighted_end - line.start..end],
                run.style,
            );
        } else {
            push_styled_slice(&mut parts, &line.text[start..end], run.style);
        }
    }
    Line::from(
        parts
            .into_iter()
            .map(|(text, style)| Span::styled(text, style))
            .collect::<Vec<_>>(),
    )
}

fn push_styled_slice(parts: &mut Vec<(String, Style)>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some((previous, previous_style)) = parts.last_mut()
        && *previous_style == style
    {
        previous.push_str(text);
    } else {
        parts.push((text.to_owned(), style));
    }
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
    code_block: Option<CodeBlockState>,
}

#[derive(Clone, Debug)]
struct CodeBlockState {
    _language: Option<String>,
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
            code_block: None,
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
                    self.code_block = Some(CodeBlockState {
                        _language: match kind {
                            CodeBlockKind::Fenced(language) if !language.is_empty() => {
                                Some(language.to_string())
                            }
                            _ => None,
                        },
                    });
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
                    self.code_block = None;
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

fn render_tool(
    tool: &ToolDisplay,
    expanded: bool,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    interactions: &mut Vec<Option<InteractionTarget>>,
) {
    let marker = match (expanded, &tool.status) {
        (true, _) => "▾",
        (false, ToolDisplayStatus::Running) => "◌",
        (false, _) => "▸",
    };
    let status = match tool.status {
        ToolDisplayStatus::Running => "",
        ToolDisplayStatus::Completed => "  ✓",
        ToolDisplayStatus::Failed | ToolDisplayStatus::Rejected => "  ✗",
    };
    let name = tool_display_name(&tool.name);
    let fixed_width = UnicodeWidthStr::width(marker)
        .saturating_add(1)
        .saturating_add(UnicodeWidthStr::width(name.as_str()))
        .saturating_add(UnicodeWidthStr::width(status));
    let summary = tool_compact_summary(
        &tool.name,
        &tool.arguments,
        width.saturating_sub(fixed_width.saturating_add(2)),
    );
    let summary = if summary.is_empty() {
        String::new()
    } else {
        format!("  {summary}")
    };
    push_rendered_line(
        lines,
        interactions,
        Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(Color::DarkGray)),
            Span::styled(
                name,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(summary),
            Span::styled(
                status,
                Style::default().fg(match tool.status {
                    ToolDisplayStatus::Completed => Color::Green,
                    ToolDisplayStatus::Failed | ToolDisplayStatus::Rejected => Color::Red,
                    ToolDisplayStatus::Running => Color::Yellow,
                }),
            ),
        ]),
        Some(InteractionTarget::Tool(tool.call_id.clone())),
    );
    if !expanded {
        return;
    }
    push_rendered_line(lines, interactions, Line::from("  参数"), None);
    if let Some(arguments) = tool.arguments.as_object() {
        for (key, value) in arguments {
            push_rendered_line(
                lines,
                interactions,
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        format!("{}：", argument_label(key)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(human_argument(key, value)),
                ]),
                None,
            );
        }
    } else {
        push_rendered_line(lines, interactions, Line::from("    （无）"), None);
    }
    push_rendered_line(lines, interactions, Line::from("  结果"), None);
    match tool.result.as_deref() {
        Some(result) if !result.is_empty() => {
            for line in result.lines() {
                push_rendered_line(
                    lines,
                    interactions,
                    Line::from(vec![
                        Span::raw("    "),
                        Span::styled(secrets::redact(line), code_style()),
                    ]),
                    None,
                );
            }
        }
        Some(_) => push_rendered_line(lines, interactions, Line::from("    （空）"), None),
        None => push_rendered_line(lines, interactions, Line::from("    执行中…"), None),
    }
}

fn render_thinking_summary(
    thinking: &ThinkingDisplay,
    expanded: bool,
    expanded_body: Option<&[Line<'static>]>,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    interactions: &mut Vec<Option<InteractionTarget>>,
) {
    let theme = UiTheme::default();
    let marker = if expanded { "▾" } else { "▸" };
    let label = "思考摘要";
    let interaction = Some(InteractionTarget::ThinkingSummary(thinking.id.clone()));
    if !expanded {
        let last_line = thinking
            .content
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        let fixed_width = UnicodeWidthStr::width(marker)
            .saturating_add(1)
            .saturating_add(UnicodeWidthStr::width(label));
        let summary = fit_text_tail(
            last_line,
            width.saturating_sub(fixed_width.saturating_add(2)),
        );
        push_rendered_line(
            lines,
            interactions,
            Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(Color::DarkGray)),
                Span::styled(label, theme.strong(VisualRole::Thinking)),
                Span::raw(if summary.is_empty() {
                    String::new()
                } else {
                    format!("  {summary}")
                }),
            ]),
            interaction,
        );
        return;
    }
    push_rendered_line(
        lines,
        interactions,
        Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(Color::DarkGray)),
            Span::styled(label, theme.strong(VisualRole::Thinking)),
        ]),
        interaction,
    );
    if thinking.content.trim().is_empty() {
        push_rendered_line(
            lines,
            interactions,
            Line::from(Span::styled("  （空）", theme.style(VisualRole::Muted))),
            None,
        );
        return;
    }
    if let Some(body) = expanded_body {
        for line in body {
            push_rendered_line(lines, interactions, line.clone(), None);
        }
    } else {
        for line in render_markdown(&thinking.content, theme.style(VisualRole::Primary)) {
            push_rendered_line(lines, interactions, line, None);
        }
    }
}

pub(crate) fn tool_display_name(name: &str) -> String {
    let translated = match name {
        "file_list" => Some("文件列表"),
        "file_stat" => Some("文件信息"),
        "file_read" => Some("文件读取"),
        "file_search" => Some("文件搜索"),
        "file_mkdir" => Some("新建目录"),
        "file_write" => Some("文件修改"),
        "file_copy" => Some("文件复制"),
        "file_move" => Some("文件移动"),
        "file_delete" => Some("文件删除"),
        "web_search" => Some("网络搜索"),
        "web_fetch" | "webfetch" => Some("网页读取"),
        "terminal_exec" => Some("命令执行"),
        "terminal_shell" => Some("Shell 命令"),
        "agent_spawn" => Some("子 Agent"),
        "git" => Some("Git 操作"),
        "git_diff" => Some("差异查看"),
        "browser_open" => Some("打开网页"),
        "browser_snapshot" => Some("页面快照"),
        "browser_click" => Some("页面点击"),
        "browser_type" => Some("页面输入"),
        "browser_press" => Some("页面按键"),
        _ => None,
    };
    if let Some(translated) = translated {
        return translated.to_owned();
    }
    if let Some(external) = name.strip_prefix("mcp:") {
        let tool = external.rsplit([':', '/']).next().unwrap_or(external);
        return format!("外部工具：{}", tool.replace('_', " "));
    }
    name.replace('_', " ")
}

fn tool_compact_summary(name: &str, arguments: &Value, width: usize) -> String {
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| arguments.get(*key).and_then(Value::as_str))
            .unwrap_or_default()
    };
    let raw = match name {
        "file_read" | "file_write" | "file_stat" | "file_list" | "file_mkdir" | "file_delete" => {
            get(&["path"]).to_owned()
        }
        "file_search" => [get(&["path"]), get(&["query", "pattern"])]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("  "),
        "web_search" => get(&["query"]).to_owned(),
        "web_fetch" | "webfetch" | "browser_open" => get(&["url"]).to_owned(),
        "terminal_exec" => {
            let mut parts = Vec::new();
            let program = get(&["program", "command"]);
            if !program.is_empty() {
                parts.push(program.to_owned());
            }
            if let Some(args) = arguments.get("args").and_then(Value::as_array) {
                parts.extend(
                    args.iter()
                        .filter_map(Value::as_str)
                        .take(4)
                        .map(str::to_owned),
                );
            }
            parts.join(" ")
        }
        "terminal_shell" => get(&["command"]).to_owned(),
        "git" | "git_diff" => arguments
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        "file_move" | "file_copy" => format!(
            "{} -> {}",
            get(&["source", "from"]),
            get(&["destination", "to"])
        ),
        "agent_spawn" => get(&["prompt", "task"]).to_owned(),
        _ => get(&["path", "query", "url"]).to_owned(),
    };
    fit_text_tail(&secrets::redact(raw.trim()), width)
}

fn fit_text_tail(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    let ellipsis = if width > 1 { "…" } else { "" };
    let target = width.saturating_sub(UnicodeWidthStr::width(ellipsis));
    let mut kept = Vec::new();
    let mut used = 0usize;
    for grapheme in value.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(grapheme_width) > target {
            break;
        }
        kept.push(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    kept.reverse();
    format!("{ellipsis}{}", kept.concat())
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
    UiTheme::default().style(VisualRole::Code)
}

/// Clickable rectangle for the mode portion of the input title, which is
/// rendered left-aligned on the top border as `" 输入 · {模式} "`.
fn input_mode_rect(area: Rect, mode: AgentMode) -> Option<Rect> {
    const PREFIX: &str = " 输入 · ";
    let label = mode_label(mode);
    let prefix_width = UnicodeWidthStr::width(PREFIX) as u16;
    let label_width = UnicodeWidthStr::width(label) as u16;
    if label_width == 0 {
        return None;
    }
    let x = area.x.saturating_add(1).saturating_add(prefix_width);
    if x.saturating_add(label_width) > area.right().saturating_sub(1) {
        return None;
    }
    Some(Rect::new(x, area.y, label_width, 1))
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &mut App, view: &InputView, theme: &UiTheme) {
    app.input_mode_rect = input_mode_rect(area, app.current.mode);
    let style = if view.enabled {
        theme.style(VisualRole::Primary)
    } else {
        theme.style(VisualRole::Muted)
    };
    let border_style = if view.warning {
        theme.style(VisualRole::Warning)
    } else if view.enabled {
        theme.focus_border
    } else {
        theme.inactive_border
    };
    let inner_width = area.width.saturating_sub(2) as usize;
    let viewport = input_cursor_viewport(app.input.as_str(), app.input.cursor(), inner_width);
    frame.render_widget(
        Paragraph::new(viewport.text).style(style).block(
            Block::default()
                .title(view.title.as_str())
                .borders(Borders::ALL)
                .border_style(border_style),
        ),
        area,
    );
    if !app.current.busy && app.settings.is_none() && !app.has_pending_approval() {
        let cursor_x = area.x + 1 + viewport.cursor_column as u16;
        let cursor_y = area.y + 1 + viewport.cursor_row.min(area.height.saturating_sub(3));
        frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(1)), cursor_y));
    }
}

fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &UiViewModel,
    app: &mut App,
    theme: &UiTheme,
) {
    let mut lines = vec![footer_line(
        &view.footer.primary,
        area.width as usize,
        theme,
    )];
    if area.height > 1
        && let Some(secondary) = &view.footer.secondary
    {
        lines.push(footer_line(secondary, area.width as usize, theme));
    }
    frame.render_widget(Paragraph::new(lines), area);
    app.thinking_control_rect = thinking_control_rect(area, view);
    (app.provider_control_rect, app.model_control_rect) = provider_model_rects(area, view, app);
}

fn provider_model_rects(area: Rect, view: &UiViewModel, app: &App) -> (Option<Rect>, Option<Rect>) {
    let Some(secondary) = view.footer.secondary.as_ref() else {
        return (None, None);
    };
    let right = clip_segments(&secondary.right, area.width as usize);
    let right_width = segment_width(&right);
    let left_budget = (area.width as usize)
        .saturating_sub(right_width.saturating_add(usize::from(right_width > 0)));
    let left = clip_segments(&secondary.left, left_budget);
    let Some(text) = left.first().map(|segment| segment.text.as_str()) else {
        return (None, None);
    };
    let prefix = format!("{} · ", mode_label(app.current.mode));
    if !text.starts_with(&prefix) || area.height < 2 {
        return (None, None);
    }
    let prefix_width = UnicodeWidthStr::width(prefix.as_str()) as u16;
    let visible_width = UnicodeWidthStr::width(text) as u16;
    let provider_width = UnicodeWidthStr::width(app.provider_label()) as u16;
    let separator_width = UnicodeWidthStr::width(" · ") as u16;
    let model_width = UnicodeWidthStr::width(app.model_name()) as u16;
    let provider_x = area.x.saturating_add(prefix_width);
    let model_x = provider_x
        .saturating_add(provider_width)
        .saturating_add(separator_width);
    let provider = (provider_width > 0
        && visible_width >= prefix_width.saturating_add(provider_width))
    .then(|| Rect::new(provider_x, area.y.saturating_add(1), provider_width, 1));
    let model = (model_width > 0
        && visible_width
            >= prefix_width
                .saturating_add(provider_width)
                .saturating_add(separator_width)
                .saturating_add(model_width))
    .then(|| Rect::new(model_x, area.y.saturating_add(1), model_width, 1));
    (provider, model)
}

fn draw_provider_menu(
    frame: &mut Frame<'_>,
    screen: Rect,
    footer: Rect,
    app: &mut App,
    theme: &UiTheme,
) {
    let choices = crate::app::provider_choices(app);
    let content_width = choices
        .iter()
        .map(|preset| UnicodeWidthStr::width(preset.label()))
        .max()
        .unwrap_or(12)
        .saturating_add(4) as u16;
    let width = content_width.clamp(20, 36).min(screen.width);
    let height = (choices.len() as u16).saturating_add(2).min(screen.height);
    let control = app
        .provider_control_rect
        .unwrap_or(Rect::new(footer.x, footer.y, 0, 0));
    let x = control.x.min(screen.right().saturating_sub(width));
    let y = footer.y.saturating_sub(height);
    let area = Rect::new(x, y, width, height);
    app.provider_menu_rect = Some(area);

    let items = choices
        .iter()
        .enumerate()
        .map(|(index, preset)| {
            let selected = index == app.provider_menu_selected;
            ListItem::new(Line::from(Span::styled(
                format!("{} {}", if selected { "›" } else { " " }, preset.label()),
                if selected {
                    theme.selected
                } else {
                    theme.style(VisualRole::Primary)
                },
            )))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(" 选择供应商 ")
                .borders(Borders::ALL)
                .border_style(theme.focus_border),
        ),
        area,
    );
}

fn draw_model_menu(
    frame: &mut Frame<'_>,
    screen: Rect,
    footer: Rect,
    app: &mut App,
    theme: &UiTheme,
) {
    let choices = crate::app::model_choices(app);
    let content_width = choices
        .iter()
        .map(|model| UnicodeWidthStr::width(model.as_str()))
        .max()
        .unwrap_or(12)
        .saturating_add(4) as u16;
    let width = content_width.clamp(24, 52).min(screen.width);
    let height = (choices.len() as u16)
        .saturating_add(2)
        .min(14)
        .min(screen.height);
    let control = app
        .model_control_rect
        .unwrap_or(Rect::new(footer.x, footer.y, 0, 0));
    let x = control.x.min(screen.right().saturating_sub(width));
    let y = footer.y.saturating_sub(height);
    let area = Rect::new(x, y, width, height);
    app.model_menu_rect = Some(area);

    let visible = area.height.saturating_sub(2) as usize;
    let scroll = app
        .model_menu_selected
        .saturating_sub(visible.saturating_sub(1));
    let items = choices
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(index, model)| {
            let selected = index == app.model_menu_selected;
            ListItem::new(Line::from(Span::styled(
                format!("{} {}", if selected { "›" } else { " " }, model),
                if selected {
                    theme.selected
                } else {
                    theme.style(VisualRole::Primary)
                },
            )))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(" {} 模型 ", app.provider_label()))
                .borders(Borders::ALL)
                .border_style(theme.focus_border),
        ),
        area,
    );
}

fn thinking_control_rect(area: Rect, view: &UiViewModel) -> Option<Rect> {
    if area.height < 2 || view.footer.secondary.is_none() {
        return None;
    }
    let width = UnicodeWidthStr::width(view.thinking.label.as_str()) as u16;
    (width > 0 && width <= area.width).then(|| {
        Rect::new(
            area.right().saturating_sub(width),
            area.y.saturating_add(1),
            width,
            1,
        )
    })
}

fn draw_thinking_menu(
    frame: &mut Frame<'_>,
    screen: Rect,
    footer: Rect,
    app: &mut App,
    view: &ThinkingControlView,
    theme: &UiTheme,
) {
    let rows = view
        .options
        .len()
        .max(if view.qwen37_budgets { 6 } else { 0 });
    let width = if view.qwen37_budgets { 28 } else { 18 }.min(screen.width);
    let height = (rows as u16).saturating_add(2).min(screen.height);
    let control = app
        .thinking_control_rect
        .unwrap_or(Rect::new(footer.x, footer.y, 0, 0));
    let x = control
        .right()
        .saturating_sub(width)
        .min(screen.right().saturating_sub(width));
    let y = footer.y.saturating_sub(height);
    let area = Rect::new(x, y, width, height);
    app.thinking_menu_rect = Some(area);

    let mut lines = Vec::with_capacity(rows);
    for index in 0..rows {
        let left = view.options.get(index).map_or_else(String::new, |item| {
            format!("{} {}", if item.selected { "●" } else { "○" }, item.label)
        });
        let text = if view.qwen37_budgets {
            const BUDGETS: [(Option<u32>, &str); 6] = [
                (None, "默认"),
                (Some(1024), "1k"),
                (Some(4096), "4k"),
                (Some(8192), "8k"),
                (Some(16384), "16k"),
                (Some(32768), "32k"),
            ];
            let (budget, label) = BUDGETS.get(index).copied().unwrap_or((None, ""));
            format!(
                "{left:<8} {} {label}",
                if view.budget_tokens == budget
                    && app.thinking_level() == crate::config::ThinkingLevel::Enabled
                {
                    "●"
                } else {
                    "○"
                }
            )
        } else {
            left
        };
        lines.push(Line::from(Span::styled(
            text,
            theme.style(VisualRole::Primary),
        )));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" 思考强度 ")
                .border_style(theme.style(VisualRole::Accent)),
        ),
        area,
    );
}

fn footer_line(view: &FooterLine, width: usize, theme: &UiTheme) -> Line<'static> {
    let right = clip_segments(&view.right, width);
    let right_width = segment_width(&right);
    let left_budget =
        width.saturating_sub(right_width.saturating_add(usize::from(right_width > 0)));
    let left = clip_segments(&view.left, left_budget);
    let left_width = segment_width(&left);
    let gap = width.saturating_sub(left_width.saturating_add(right_width));
    let mut spans = render_segments(&left, theme);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(render_segments(&right, theme));
    Line::from(spans)
}

fn clip_segments(segments: &[UiSegment], width: usize) -> Vec<UiSegment> {
    let mut output = Vec::new();
    let mut remaining = width;
    for segment in segments {
        if remaining == 0 {
            break;
        }
        let segment_width = UnicodeWidthStr::width(segment.text.as_str());
        if segment_width <= remaining {
            output.push(segment.clone());
            remaining -= segment_width;
            continue;
        }
        let mut text = String::new();
        let mut used = 0usize;
        for grapheme in segment.text.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if used.saturating_add(grapheme_width) > remaining {
                break;
            }
            text.push_str(grapheme);
            used = used.saturating_add(grapheme_width);
        }
        if !text.is_empty() {
            output.push(UiSegment {
                text,
                role: segment.role,
            });
        }
        break;
    }
    output
}

fn segment_width(segments: &[UiSegment]) -> usize {
    segments
        .iter()
        .map(|segment| UnicodeWidthStr::width(segment.text.as_str()))
        .sum()
}

fn render_segments(segments: &[UiSegment], theme: &UiTheme) -> Vec<Span<'static>> {
    segments
        .iter()
        .map(|segment| {
            let style = if matches!(segment.role, VisualRole::Primary | VisualRole::Shortcut) {
                theme.strong(segment.role)
            } else {
                theme.style(segment.role)
            };
            Span::styled(segment.text.clone(), style)
        })
        .collect()
}

fn draw_approval(frame: &mut Frame<'_>, area: Rect, app: &App, theme: &UiTheme) {
    let popup = centered_rect(76, 18, area);
    let approval = app.pending_approval().expect("approval exists");
    let mut lines = approval_lines(&approval.call, &approval.reason);
    if let Some(title) = approval.source_title.as_deref() {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("来源  子 Agent · {title}"),
                Style::default().fg(Color::Cyan),
            )),
        );
    }
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
                    .border_style(theme.style(VisualRole::Warning)),
            ),
        popup,
    );
}

fn approval_lines(call: &crate::provider::ToolCall, reason: &str) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("工具  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                tool_display_name(&call.name),
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
        "path" => "路径",
        "source" | "from" => "来源",
        "destination" | "to" => "目标",
        "command" => "命令",
        "args" => "参数",
        "cwd" => "目录",
        "url" => "URL",
        "content" => "内容",
        "max_bytes" => "最大大小",
        "timeout_seconds" => "超时",
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

/// One row in the settings list: a section header, an editable field, or a
/// breathing-space spacer. Derived from the `FIELDS` registry.
enum SettingsRow {
    Section(&'static str),
    Field(SettingsField),
    Spacer,
}

fn settings_rows() -> Vec<SettingsRow> {
    let mut rows = Vec::with_capacity(FIELDS.len() * 2 + 3);
    let mut last_section = None;
    for spec in FIELDS {
        if last_section != Some(spec.section) {
            last_section = Some(spec.section);
            rows.push(SettingsRow::Section(spec.section));
            rows.push(SettingsRow::Spacer);
        }
        rows.push(SettingsRow::Field(spec.field));
        rows.push(SettingsRow::Spacer);
    }
    rows
}

const SETTINGS_LABEL_COLUMNS: usize = 12;

fn draw_settings(frame: &mut Frame<'_>, area: Rect, settings: &SettingsState, theme: &UiTheme) {
    match settings {
        SettingsState::List(list) => draw_provider_list(frame, area, list, theme),
        SettingsState::Templates(templates) => {
            draw_provider_templates(frame, area, templates, theme)
        }
        SettingsState::Form(form) => draw_provider_form(frame, area, form, theme),
    }
}

fn draw_provider_list(
    frame: &mut Frame<'_>,
    area: Rect,
    list: &crate::settings::ProviderList,
    theme: &UiTheme,
) {
    let popup = centered_rect(78, 20, area);
    let mut lines = vec![
        Line::from(Span::styled(
            "  已连接的供应商",
            theme.strong(VisualRole::Accent),
        )),
        Line::default(),
    ];
    for (index, provider) in list.providers.iter().enumerate() {
        let selected = index == list.selected;
        let current = if provider.preset == list.active {
            " 当前"
        } else {
            ""
        };
        let status = if list.connected.contains(&provider.preset) {
            "已连接"
        } else {
            "密钥未解锁"
        };
        lines.push(Line::from(Span::styled(
            format!(
                "  {} {:<20} {:<26} {status}{current}",
                if selected { "›" } else { " " },
                provider.preset.label(),
                provider.model
            ),
            if selected {
                theme.selected
            } else {
                theme.style(VisualRole::Primary)
            },
        )));
    }
    let selected = list.selected == list.providers.len();
    lines.extend([
        Line::default(),
        Line::from(Span::styled(
            format!("  {} 添加供应商", if selected { "›" } else { " " }),
            if selected {
                theme.selected
            } else {
                theme.strong(VisualRole::Success)
            },
        )),
        Line::default(),
        Line::from(Span::styled(
            "  ↑/↓ 选择  Enter 编辑或添加  Esc 关闭",
            theme.style(VisualRole::Muted),
        )),
    ]);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" 供应商连接 ")
                .borders(Borders::ALL)
                .border_style(theme.focus_border),
        ),
        popup,
    );
}

fn draw_provider_templates(
    frame: &mut Frame<'_>,
    area: Rect,
    templates: &crate::settings::TemplateList,
    theme: &UiTheme,
) {
    let popup = centered_rect(68, 18, area);
    let mut lines = vec![
        Line::from(Span::styled(
            "  选择供应商模板",
            theme.strong(VisualRole::Accent),
        )),
        Line::default(),
    ];
    if templates.presets.is_empty() {
        lines.push(Line::from(Span::styled(
            "  所有供应商模板均已添加",
            theme.style(VisualRole::Muted),
        )));
    }
    for (index, preset) in templates.presets.iter().enumerate() {
        let selected = index == templates.selected;
        lines.push(Line::from(Span::styled(
            format!("  {} {}", if selected { "›" } else { " " }, preset.label()),
            if selected {
                theme.selected
            } else {
                theme.style(VisualRole::Primary)
            },
        )));
    }
    lines.extend([
        Line::default(),
        Line::from(Span::styled(
            "  ↑/↓ 选择  Enter 继续  Esc 返回",
            theme.style(VisualRole::Muted),
        )),
    ]);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" 添加供应商 ")
                .borders(Borders::ALL)
                .border_style(theme.focus_border),
        ),
        popup,
    );
}

fn draw_provider_form(frame: &mut Frame<'_>, area: Rect, form: &SettingsForm, theme: &UiTheme) {
    let popup = centered_rect(88, 22, area);
    let inner = Block::bordered().inner(popup);
    let footer_rows = 2usize;
    let visible = (inner.height as usize).saturating_sub(footer_rows);

    let rows = settings_rows();
    let selected_field = form.field();
    let selected_row = rows
        .iter()
        .position(|row| matches!(row, SettingsRow::Field(field) if *field == selected_field))
        .unwrap_or(0);
    let scroll = selected_row
        .saturating_sub(visible.saturating_sub(1))
        .min(rows.len().saturating_sub(visible));

    let value_width = inner
        .width
        .saturating_sub(SETTINGS_LABEL_COLUMNS as u16 + 7) as usize;
    let mut lines = Vec::with_capacity(visible + footer_rows);
    for row in rows.iter().skip(scroll).take(visible) {
        match row {
            SettingsRow::Section(section) => {
                let fill = inner
                    .width
                    .saturating_sub(UnicodeWidthStr::width(*section) as u16 + 6)
                    .min(24) as usize;
                lines.push(Line::from(Span::styled(
                    format!("  ━━ {section} {}", "━".repeat(fill)),
                    theme.strong(VisualRole::Accent),
                )));
            }
            SettingsRow::Field(field) => {
                let spec = FIELDS.iter().find(|spec| spec.field == *field).unwrap();
                let selected = *field == selected_field;
                let marker = if selected { "›" } else { " " };
                let label_pad = " ".repeat(
                    SETTINGS_LABEL_COLUMNS.saturating_sub(UnicodeWidthStr::width(spec.label)),
                );
                let value = form.value(*field);
                let label_style = if selected {
                    theme.selected
                } else {
                    theme.style(VisualRole::Primary)
                };
                let value_style = if selected {
                    theme.selected
                } else {
                    theme.style(VisualRole::Secondary)
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {marker} {}{label_pad}", spec.label), label_style),
                    Span::styled(
                        format!("  {}", fit_text(value.as_ref(), value_width)),
                        value_style,
                    ),
                ]));
            }
            SettingsRow::Spacer => lines.push(Line::default()),
        }
    }
    let divider = "─".repeat(inner.width.saturating_sub(2) as usize);
    lines.push(Line::from(Span::styled(
        format!("  {divider}"),
        theme.style(VisualRole::Muted),
    )));
    lines.push(Line::from(vec![
        Span::styled("  ↑/↓ 选择  ", theme.strong(VisualRole::Success)),
        Span::styled("←/→ 修改  ", theme.strong(VisualRole::Success)),
        Span::styled("Enter 保存  ", theme.strong(VisualRole::Success)),
        Span::styled("Ctrl+D 移除  ", theme.strong(VisualRole::Success)),
        Span::styled("Esc 返回", theme.strong(VisualRole::Success)),
    ]));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" 编辑 {} ", form.provider.preset.label()))
                    .borders(Borders::ALL)
                    .border_style(theme.focus_border),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_palette(frame: &mut Frame<'_>, area: Rect, palette: &CommandPaletteState, theme: &UiTheme) {
    let popup = centered_rect(84, 16, area);
    let matches = commands::matches(&palette.query, 10);
    let outer = Block::default()
        .title(" 命令面板 · Ctrl+P / Ctrl+X ")
        .borders(Borders::ALL)
        .border_style(theme.focus_border);
    let inner = outer.inner(popup);
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    let columns =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).split(rows[1]);
    let label_columns = commands::PALETTE_ITEMS
        .iter()
        .map(|item| UnicodeWidthStr::width(item.label))
        .max()
        .unwrap_or_default();
    let query = Line::from(vec![
        Span::styled("/", Style::default().fg(Color::Cyan)),
        Span::raw(palette.query.clone()),
    ]);
    let mut lines = Vec::new();
    for (index, item) in matches.iter().enumerate() {
        let selected = index == palette.selected;
        let style = if selected {
            theme.selected
        } else {
            theme.style(VisualRole::Primary)
        };
        let item = &commands::PALETTE_ITEMS[item.index];
        lines.push(Line::from(Span::styled(
            palette_item_text(item, selected, label_columns),
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
    frame.render_widget(outer, popup);
    frame.render_widget(Paragraph::new(query), rows[0]);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), columns[0]);

    let detail = matches
        .get(palette.selected)
        .map(|item| &commands::PALETTE_ITEMS[item.index])
        .map(|item| {
            let command = item.command.unwrap_or("直接动作");
            vec![
                Line::from(Span::styled(item.label, theme.strong(VisualRole::Success))),
                Line::default(),
                Line::from(item.description),
                Line::default(),
                Line::from(Span::styled(command, Style::default().fg(Color::Cyan))),
                Line::default(),
                Line::from(Span::styled(
                    "↑/↓ 选择 · Enter 执行 · Esc 关闭",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        })
        .unwrap_or_else(|| {
            vec![Line::from(Span::styled(
                "输入命令名或功能名称进行筛选",
                Style::default().fg(Color::DarkGray),
            ))]
        });
    frame.render_widget(
        Paragraph::new(detail)
            .block(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(theme.focus_border),
            )
            .wrap(Wrap { trim: true }),
        columns[1],
    );
}

fn palette_item_text(item: &commands::PaletteItem, selected: bool, label_columns: usize) -> String {
    let label_padding =
        " ".repeat(label_columns.saturating_sub(UnicodeWidthStr::width(item.label)));
    let command = item.command.unwrap_or("直接动作");
    let shortcut = item
        .shortcut
        .map(|shortcut| format!(" · {shortcut}"))
        .unwrap_or_default();
    format!(
        "{} {}{label_padding}  {command}{shortcut}",
        if selected { ">" } else { " " },
        item.label,
    )
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
                UiTheme::default().selected
            } else {
                UiTheme::default().style(VisualRole::Primary)
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
                .border_style(UiTheme::default().focus_border),
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

#[cfg(test)]
pub(crate) fn live_thinking_line_with_braille(app: &App, braille: bool) -> String {
    live_thinking_lines_with_braille(app, usize::MAX, braille).join("\n")
}

struct LiveThinkingUpdate {
    title: String,
    lines: Option<Vec<String>>,
}

fn live_thinking_lines(app: &mut App, width: usize, existing_rows: usize) -> LiveThinkingUpdate {
    live_thinking_lines_cached(
        app,
        width,
        crate::app::braille_spinner_supported(),
        existing_rows,
    )
}

fn live_thinking_lines_cached(
    app: &mut App,
    width: usize,
    braille: bool,
    existing_rows: usize,
) -> LiveThinkingUpdate {
    if app.current.thinking_anchor.is_none() {
        return LiveThinkingUpdate {
            title: String::new(),
            lines: (existing_rows != 0).then(Vec::new),
        };
    }
    if !app.current.thinking_expanded {
        let lines = live_thinking_lines_with_braille(app, width, braille);
        let title = lines.into_iter().next().unwrap_or_default();
        return LiveThinkingUpdate {
            title: title.clone(),
            lines: (existing_rows != 1).then(|| vec![title]),
        };
    }

    let width = width.max(1);
    let status = if app.current.thinking_active {
        "思考中"
    } else {
        match app.current.thinking_result {
            crate::app::ThinkingResult::Completed => "思考完成",
            crate::app::ThinkingResult::Failed => "思考失败",
            crate::app::ThinkingResult::Cancelled => "思考已取消",
        }
    };
    let title = if app.current.thinking_active {
        format!(
            "▾ {} {status}",
            crate::app::thinking_animation_glyph(app.current.thinking_animation_frame, braille)
        )
    } else {
        format!("▾ {status}")
    };
    let title = fit_text_tail(&title, width);

    let buffer = &app.current.thinking_buffer;
    let reasoning = buffer.trim();
    if reasoning.is_empty() {
        let mut lines = vec![title.clone()];
        if app.current.thinking_buffer_truncated {
            lines.extend(wrap_grapheme_lines("  [较早思考内容已截断]", width));
        }
        lines.extend(wrap_grapheme_lines(
            &format!("  {}", app.current.thinking_last_line),
            width,
        ));
        return LiveThinkingUpdate {
            title,
            lines: Some(lines),
        };
    }
    let source_start = reasoning.as_ptr() as usize - buffer.as_ptr() as usize;
    let epoch = app.current.thinking_buffer_epoch;
    let cache = &mut app.current.live_thinking_layout_cache;
    let prefix_unchanged = cache.width == width
        && cache.buffer_epoch == epoch
        && cache.source_start == source_start
        && cache.processed_len <= reasoning.len()
        && reasoning.is_char_boundary(cache.processed_len);
    let mut body_changed = !prefix_unchanged;
    if body_changed {
        #[cfg(test)]
        let rebuilds = cache.full_rebuilds + 1;
        cache.clear();
        cache.width = width;
        cache.buffer_epoch = epoch;
        cache.source_start = source_start;
        cache.current_row = "  ".into();
        cache.current_width = 2;
        #[cfg(test)]
        {
            cache.full_rebuilds = rebuilds;
        }
    }
    let tail = &reasoning[cache.processed_len..];
    body_changed |= !tail.is_empty();
    #[cfg(test)]
    {
        cache.processed_bytes += tail.len();
    }
    append_wrapped_thinking(cache, tail, width);
    cache.processed_len = reasoning.len();
    let lines = (body_changed || existing_rows <= 1).then(|| {
        let mut lines = vec![title.clone()];
        if app.current.thinking_buffer_truncated {
            lines.extend(wrap_grapheme_lines("  [较早思考内容已截断]", width));
        }
        lines.extend(cache.rows.iter().cloned());
        lines.push(cache.current_row.clone());
        lines
    });
    LiveThinkingUpdate { title, lines }
}

fn append_wrapped_thinking(
    cache: &mut crate::session::LiveThinkingLayoutCache,
    value: &str,
    width: usize,
) {
    for grapheme in value.graphemes(true) {
        if grapheme == "\n" || grapheme == "\r\n" {
            cache.rows.push(std::mem::take(&mut cache.current_row));
            cache.current_row = "  ".into();
            cache.current_width = 2;
            continue;
        }
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if cache.current_width > 0 && cache.current_width.saturating_add(grapheme_width) > width {
            cache.rows.push(std::mem::take(&mut cache.current_row));
            cache.current_width = 0;
        }
        cache.current_row.push_str(grapheme);
        cache.current_width = cache.current_width.saturating_add(grapheme_width);
    }
}

fn live_thinking_lines_with_braille(app: &App, width: usize, braille: bool) -> Vec<String> {
    if app.current.thinking_anchor.is_none() {
        return Vec::new();
    }
    let status = if app.current.thinking_active {
        "思考中"
    } else {
        match app.current.thinking_result {
            crate::app::ThinkingResult::Completed => "思考完成",
            crate::app::ThinkingResult::Failed => "思考失败",
            crate::app::ThinkingResult::Cancelled => "思考已取消",
        }
    };
    if !app.current.thinking_expanded {
        let prefix = if app.current.thinking_active {
            crate::app::thinking_animation_glyph(app.current.thinking_animation_frame, braille)
                .to_string()
        } else {
            match app.current.thinking_result {
                crate::app::ThinkingResult::Completed => "✓",
                crate::app::ThinkingResult::Failed => "✗",
                crate::app::ThinkingResult::Cancelled => "■",
            }
            .to_owned()
        };
        let suffix = app.current.thinking_last_line.trim();
        let fixed = format!("{prefix} {status}");
        let available = width
            .saturating_sub(UnicodeWidthStr::width(fixed.as_str()))
            .saturating_sub(2);
        let suffix = fit_text_tail(suffix, available);
        return vec![if suffix.is_empty()
            || (app.current.thinking_active && suffix == "模型正在思考")
        {
            fixed
        } else {
            format!("{fixed}  {suffix}")
        }];
    }
    let expanded_title = if app.current.thinking_active {
        format!(
            "▾ {} {status}",
            crate::app::thinking_animation_glyph(app.current.thinking_animation_frame, braille)
        )
    } else {
        format!("▾ {status}")
    };
    let mut lines = vec![fit_text_tail(&expanded_title, width)];
    if app.current.thinking_buffer_truncated {
        lines.extend(wrap_grapheme_lines("  [较早思考内容已截断]", width));
    }
    let reasoning = app.current.thinking_buffer.trim();
    if reasoning.is_empty() {
        lines.extend(wrap_grapheme_lines(
            &format!("  {}", app.current.thinking_last_line),
            width,
        ));
    } else {
        for line in reasoning.lines() {
            lines.extend(wrap_grapheme_lines(&format!("  {line}"), width));
        }
    }
    lines
}

fn wrap_grapheme_lines(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut output = Vec::new();
    for logical_line in value.split('\n') {
        if logical_line.is_empty() {
            output.push(String::new());
            continue;
        }
        let mut row = String::new();
        let mut used = 0usize;
        for grapheme in logical_line.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if !row.is_empty() && used.saturating_add(grapheme_width) > width {
                output.push(row);
                row = String::new();
                used = 0;
            }
            row.push_str(grapheme);
            used = used.saturating_add(grapheme_width);
        }
        output.push(row);
    }
    output
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
    fn palette_commands_align_after_mixed_width_labels() {
        let label_columns = commands::PALETTE_ITEMS
            .iter()
            .map(|item| UnicodeWidthStr::width(item.label))
            .max()
            .unwrap();
        let command_columns = commands::PALETTE_ITEMS
            .iter()
            .filter(|item| item.command.is_some())
            .map(|item| {
                let text = palette_item_text(item, false, label_columns);
                UnicodeWidthStr::width(&text[..text.find('/').unwrap()])
            })
            .collect::<HashSet<_>>();

        assert_eq!(command_columns.len(), 1);
    }

    #[test]
    fn long_visual_line_renders_only_the_visible_style_slices() {
        let text = "中🙂e\u{301}".repeat(6_000);
        let layout = MessageLayout::new(
            vec![Line::from(Span::styled(
                text,
                Style::default().fg(Color::Green),
            ))],
            Rect::new(0, 0, 80, 24),
            0,
        );
        assert!(layout.visual_lines.len() > 100);
        let visual = &layout.visual_lines[layout.visual_lines.len() / 2];
        let rendered = render_visual_line(&layout, visual, None, &UiTheme::default());
        assert_eq!(rendered.spans.len(), 1);

        let line = &layout.lines[visual.logical_line];
        let local_start = visual.start - line.start;
        let first = line.text[local_start..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(offset, _)| visual.start + offset)
            .unwrap();
        let selected = render_visual_line(
            &layout,
            visual,
            Some(OutputSelection {
                anchor: first,
                active: visual.end,
                dragging: false,
            }),
            &UiTheme::default(),
        );
        assert!(selected.spans.len() <= 2);
    }

    #[test]
    fn thinking_summary_folds_to_last_line_and_expands() {
        let thinking = ThinkingDisplay {
            id: "thinking-0".into(),
            content: "第一行\n\n最后一行".into(),
        };
        let target = Some(InteractionTarget::ThinkingSummary("thinking-0".into()));

        let mut lines = Vec::new();
        let mut interactions = Vec::new();
        render_thinking_summary(&thinking, false, None, 40, &mut lines, &mut interactions);
        assert_eq!(lines.len(), 1);
        assert_eq!(interactions, vec![target.clone()]);
        let collapsed = lines[0].to_string();
        assert!(collapsed.contains('▸'));
        assert!(collapsed.contains("最后一行"));
        assert!(!collapsed.contains("第一行"));

        lines.clear();
        interactions.clear();
        render_thinking_summary(&thinking, true, None, 40, &mut lines, &mut interactions);
        assert_eq!(interactions.first(), Some(&target));
        let expanded = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded.contains('▾'));
        assert!(expanded.contains("第一行"));
        assert!(expanded.contains("最后一行"));
    }

    #[test]
    fn settings_rows_group_fields_in_order() {
        let rows = settings_rows();
        let sections: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                SettingsRow::Section(section) => Some(*section),
                SettingsRow::Field(_) | SettingsRow::Spacer => None,
            })
            .collect();
        assert_eq!(sections, vec!["基础", "连接", "高级"]);

        let fields: Vec<SettingsField> = rows
            .iter()
            .filter_map(|row| match row {
                SettingsRow::Section(_) | SettingsRow::Spacer => None,
                SettingsRow::Field(field) => Some(*field),
            })
            .collect();
        assert_eq!(
            fields,
            vec![
                SettingsField::Preset,
                SettingsField::Protocol,
                SettingsField::Model,
                SettingsField::BaseUrl,
                SettingsField::Thinking,
                SettingsField::ApiKey,
            ]
        );
    }

    #[test]
    fn task_block_inner_is_the_message_layout_viewport() {
        let area = Rect::new(30, 0, 80, 20);
        let block = Block::default().title(" 任务 ").borders(Borders::BOTTOM);
        let viewport = block.inner(area);
        assert_eq!(viewport.x, area.x);
        assert_eq!(viewport.y, area.y + 1);
        assert_eq!(viewport.height, area.height - 2);

        let layout = MessageLayout::new(vec![Line::from("正文")], viewport, 0);
        assert_eq!(layout.viewport, block.inner(area));

        let tiny = Block::default()
            .title(" 任务 ")
            .borders(Borders::BOTTOM)
            .inner(Rect::new(0, 0, 1, 1));
        assert_eq!(tiny.height, 0);
    }

    #[test]
    fn session_window_start_keeps_current_visible_and_bounded() {
        assert_eq!(session_window_start(3, 0, 4), 0);
        assert_eq!(session_window_start(10, 0, 4), 0);
        assert_eq!(session_window_start(10, 3, 4), 0);
        assert_eq!(session_window_start(10, 4, 4), 1);
        assert_eq!(session_window_start(10, 9, 4), 6);
        assert_eq!(session_window_start(2, 1, 4), 0);
    }

    #[test]
    fn flatten_session_tree_defaults_collapsed_only_click_expands() {
        let sessions = vec![
            SessionSummary {
                id: "root".into(),
                title: "Root".into(),
                parent_id: None,
            },
            SessionSummary {
                id: "other".into(),
                title: "Other".into(),
                parent_id: None,
            },
            SessionSummary {
                id: "child".into(),
                title: "Child".into(),
                parent_id: Some("root".into()),
            },
        ];
        // Default: nothing explicitly expanded -> everything collapsed.
        let rows = flatten_session_tree(&sessions, &HashSet::new());
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "other"]);
        assert!(rows[0].has_children && !rows[0].expanded);

        // Explicit (click) expansion shows the children.
        let expanded: HashSet<String> = ["root".into()].into_iter().collect();
        let rows = flatten_session_tree(&sessions, &expanded);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "child", "other"]);
        assert_eq!(rows[1].depth, 1);
        assert!(rows[0].expanded);
    }

    #[test]
    fn session_index_at_maps_only_visible_session_rows() {
        let area = Rect::new(0, 0, 30, 12);
        // Panel title row 0 and header rows 1..4 are not sessions.
        assert_eq!(session_index_at(area, 1, 0, 10, 0), None);
        assert_eq!(session_index_at(area, 1, 1, 10, 0), None);
        assert_eq!(session_index_at(area, 1, 4, 10, 0), None);
        // Six session rows are visible (12 - 1 title - 4 header - 1 trailing).
        assert_eq!(session_index_at(area, 1, 5, 10, 0), Some(0));
        assert_eq!(session_index_at(area, 1, 10, 10, 0), Some(5));
        // Trailing blank row and panel border/outside map to None.
        assert_eq!(session_index_at(area, 1, 11, 10, 0), None);
        assert_eq!(session_index_at(area, 29, 5, 10, 0), None);
        assert_eq!(session_index_at(area, 30, 5, 10, 0), None);
    }

    #[test]
    fn input_mode_rect_uses_utf8_width_and_rejects_narrow_inputs() {
        let area = Rect::new(0, 20, 40, 5);
        assert_eq!(
            input_mode_rect(area, AgentMode::Build),
            Some(Rect::new(9, 20, 4, 1))
        );
        let narrow = Rect::new(0, 20, 8, 5);
        assert_eq!(input_mode_rect(narrow, AgentMode::Build), None);
    }

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
        assert_eq!(text[3], "let value = 1;");
        assert!(!text.iter().any(|line| line.contains("[code")));
        assert!(!text.iter().any(|line| line.contains("[end code]")));
        assert!(lines[1].spans.len() >= 4);
    }

    #[test]
    fn fenced_code_preserves_literal_markdown_and_copy_text() {
        for markdown in [
            "```bash\n./1h-agent --workspace /Users/yang/Desktop\n```",
            "~~~bash\n* # [x] <b>中文🙂</b>\n~~~",
            "    one\n      two\n\n    * literal",
            "```\n  indented\n\n`backtick` # heading\n```",
        ] {
            let lines = render_markdown(markdown, Style::default());
            let text = markdown_text(&lines);
            assert!(!text.contains("[code"));
            assert!(!text.contains("[end code]"));
            assert!(!text.contains("code:"));
            assert!(lines.iter().any(|line| {
                line.spans
                    .iter()
                    .any(|span| span.style.fg == Some(Color::Gray))
            }));
        }

        let lines = render_markdown(
            "```bash\n./1h-agent --workspace /Users/yang/Desktop\n```",
            Style::default(),
        );
        let layout = MessageLayout::new(lines, Rect::new(0, 0, 80, 10), 0);
        assert!(
            layout
                .text
                .contains("./1h-agent --workspace /Users/yang/Desktop")
        );
        assert!(!layout.text.contains("bash"));
        assert!(!layout.text.contains("[code"));
    }

    #[test]
    fn unclosed_streaming_code_is_visible_without_end_marker() {
        let lines = render_markdown(
            "```rust\nfn main() {\n  println!(\"中文🙂 * # [x]\");",
            Style::default(),
        );
        let text = markdown_text(&lines);
        assert!(text.contains("fn main()"));
        assert!(text.contains("中文🙂 * # [x]"));
        assert!(!text.contains("[code"));
        assert!(!text.contains("[end code]"));
    }

    #[test]
    fn code_block_keeps_empty_lines_and_nested_indent() {
        let lines = render_markdown(
            "```text\nroot\n\n    child\n        grandchild\n```",
            Style::default(),
        );
        let text = markdown_text(&lines);
        assert!(text.contains("root\n\n    child\n        grandchild"));
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
        assert!(text.contains("fenced"));
        assert!(!text.contains("[code"));
        assert!(!text.contains("[end code]"));
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
    fn tool_output_is_merged_translated_and_structured() {
        let tool = ToolDisplay {
            call_id: "call-1".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
            status: ToolDisplayStatus::Completed,
            result: Some("line one\nline two".into()),
        };
        let mut lines = Vec::new();
        let mut interactions = Vec::new();
        render_tool(&tool, true, 80, &mut lines, &mut interactions);
        let text = markdown_text(&lines);
        assert!(text.contains("文件读取"));
        assert!(text.contains("路径：a.txt"));
        assert!(text.contains("line two"));
        assert_eq!(
            interactions[0],
            Some(InteractionTarget::Tool("call-1".into()))
        );
    }

    #[test]
    fn all_builtin_tool_names_are_translated_and_unknown_names_stay_readable() {
        let expected = [
            ("file_list", "文件列表"),
            ("file_stat", "文件信息"),
            ("file_read", "文件读取"),
            ("file_search", "文件搜索"),
            ("file_mkdir", "新建目录"),
            ("file_write", "文件修改"),
            ("file_copy", "文件复制"),
            ("file_move", "文件移动"),
            ("file_delete", "文件删除"),
            ("web_search", "网络搜索"),
            ("web_fetch", "网页读取"),
            ("terminal_exec", "命令执行"),
            ("terminal_shell", "Shell 命令"),
            ("agent_spawn", "子 Agent"),
            ("git", "Git 操作"),
            ("git_diff", "差异查看"),
            ("browser_open", "打开网页"),
            ("browser_snapshot", "页面快照"),
            ("browser_click", "页面点击"),
            ("browser_type", "页面输入"),
            ("browser_press", "页面按键"),
        ];
        for (name, translated) in expected {
            assert_eq!(tool_display_name(name), translated);
        }
        assert_eq!(tool_display_name("custom_reader"), "custom reader");
        assert_eq!(
            tool_display_name("mcp:server:remote_tool"),
            "外部工具：remote tool"
        );
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
        assert!(text.contains("Shell 命令"));
        assert!(text.contains("HIGH"));
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("secret-value"));
        assert!(!text.contains("\"command\""));
    }

    #[test]
    fn live_thinking_wraps_all_content_on_grapheme_boundaries() {
        assert_eq!(
            wrap_grapheme_lines("「正在检查项目结构」", 10),
            vec!["「正在检查", "项目结构」"]
        );
        assert_eq!(
            wrap_grapheme_lines("abc👩‍💻e\u{301}尾", 5),
            vec!["abc👩‍💻", "e\u{301}尾"]
        );
        assert_eq!(
            wrap_grapheme_lines("one\n\ntwo", 20),
            vec!["one", "", "two"]
        );
    }
}
