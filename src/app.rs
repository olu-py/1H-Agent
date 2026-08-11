use std::{
    collections::{HashMap, HashSet},
    future::pending,
    io,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    style::Print,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ignore::WalkBuilder;
use ratatui::{Terminal, backend::CrosstermBackend};
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    agent::{AgentEvent, AgentRunner},
    commands::{self, AgentMode, Command},
    config::{Config, ProviderConfig, ProviderKind, ProviderPreset},
    input::InputBuffer,
    output::{EdgeScroll, MessageLayout, OutputSelection},
    provider::{ConversationItem, OpenAiClient, Role, ToolCall, Usage},
    secrets,
    security::Workspace,
    storage::{SessionSummary, Storage},
    tools::ToolRegistry,
    ui,
};

#[derive(Clone, Debug)]
pub enum DisplayKind {
    User,
    Assistant,
    Thinking,
    Tool,
    Error,
    System,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentPhase {
    #[default]
    Idle,
    Thinking,
    StreamingText,
    WaitingApproval,
    ToolRunning,
    Completed,
    Failed,
}

impl AgentPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Thinking => "THINKING",
            Self::StreamingText => "STREAMING_TEXT",
            Self::WaitingApproval => "WAITING_APPROVAL",
            Self::ToolRunning => "TOOL_RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModelPhase {
    #[default]
    Idle,
    Streaming,
    Completed,
    Failed,
}

impl ModelPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Streaming => "STREAMING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DisplayEntry {
    pub kind: DisplayKind,
    pub content: DisplayContent,
}

#[derive(Clone, Debug)]
pub enum DisplayContent {
    Markdown(String),
    Diff(String),
    ToolCall { name: String, arguments: Value },
    ToolResult { name: String, result: String },
}

pub struct PendingApproval {
    pub call: ToolCall,
    pub reason: String,
    pub action: ApprovalAction,
    pub created_at: Instant,
}

pub enum ApprovalAction {
    Agent(oneshot::Sender<bool>),
    Shell(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsField {
    Preset,
    Protocol,
    BaseUrl,
    Model,
    ApiKey,
}

impl SettingsField {
    const ALL: [Self; 5] = [
        Self::Preset,
        Self::Protocol,
        Self::BaseUrl,
        Self::Model,
        Self::ApiKey,
    ];

    fn cycle(self, direction: i32) -> Self {
        let current = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0) as i32;
        let next = (current + direction).rem_euclid(Self::ALL.len() as i32) as usize;
        Self::ALL[next]
    }
}

#[derive(Clone, Debug)]
pub struct SettingsState {
    pub provider: ProviderConfig,
    pub api_key: String,
    pub has_existing_key: bool,
    pub field: SettingsField,
}

#[derive(Clone, Debug)]
pub struct CommandPaletteState {
    pub query: String,
    pub selected: usize,
}

pub struct App {
    pub workspace: PathBuf,
    pub input: InputBuffer,
    pub entries: Vec<DisplayEntry>,
    pub status: String,
    pub busy: bool,
    pub agent_phase: AgentPhase,
    pub model_phase: ModelPhase,
    pub thinking_summary: String,
    pub usage: Usage,
    pub context_used_tokens: u64,
    pub context_limit_tokens: Option<u64>,
    pub context_meter_enabled: bool,
    pub pending_approval: Option<PendingApproval>,
    pub settings: Option<SettingsState>,
    pub palette: Option<CommandPaletteState>,
    pub mode: AgentMode,
    pub leader_pending: bool,
    pub expanded_tools: HashSet<usize>,
    pub file_suggestions: Vec<String>,
    pub file_selected: usize,
    pub message_scroll: usize,
    pub follow_output: bool,
    pub output_scroll_top: Option<usize>,
    pub output_selection: Option<OutputSelection>,
    pub message_layout: Option<MessageLayout>,
    pub edge_scroll: EdgeScroll,
    pub session_id: String,
    pub sessions: Vec<SessionSummary>,
    conversation: Vec<ConversationItem>,
    storage: Storage,
    config: Config,
    registry: Arc<ToolRegistry>,
    runner: Option<AgentRunner>,
    active_secret: Option<(ProviderPreset, String)>,
    agent_tx: mpsc::Sender<AgentEvent>,
    agent_rx: mpsc::Receiver<AgentEvent>,
    active_task: Option<JoinHandle<()>>,
    should_quit: bool,
}

pub async fn run(workspace_path: PathBuf, config: Config) -> Result<()> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("cannot create data directory {}", config.data_dir.display()))?;
    let storage = Storage::open(&config.data_dir.join("agent.db"))?;
    let session_id = match storage.latest_session(&workspace_path)? {
        Some(session_id) => session_id,
        None => storage.create_session(&workspace_path)?,
    };
    let sessions = storage.list_sessions(&workspace_path)?;
    let mut conversation = storage.load_messages(&session_id)?;
    trim_conversation(&mut conversation);
    let workspace = Workspace::new(&workspace_path)?;
    let registry = Arc::new(ToolRegistry::new(
        workspace,
        config.runtime.clone(),
        config.security.allow_private_networks,
    ));
    registry.set_permission_rules(config.permissions.tools.clone());
    registry.set_external_config(config.browser.clone(), config.mcp_servers.clone());
    let _ = registry.initialize_mcp().await;
    let (agent_tx, agent_rx) = mpsc::channel(128);
    let (runner, active_secret, initial_status) = match secrets::api_key(config.provider.preset) {
        Ok(api_key) => {
            let provider = OpenAiClient::new(config.provider.base_url.clone(), api_key.clone())?;
            (
                Some(AgentRunner::new(
                    provider,
                    config.provider.clone(),
                    config.runtime.clone(),
                    registry.clone(),
                    storage.clone(),
                    session_id.clone(),
                )),
                Some((config.provider.preset, api_key)),
                format!(
                    "Ready | {} | {}",
                    config.provider.preset.label(),
                    config.provider.model
                ),
            )
        }
        Err(secrets::SecretError::Missing(_)) => (None, None, "需要配置提供商".into()),
        Err(error) => (
            None,
            None,
            format!(
                "系统密钥环读取失败：{}",
                secrets::redact(&error.to_string())
            ),
        ),
    };
    let entries = display_entries(&conversation);
    let initial_mode = storage
        .session_mode(&session_id)
        .ok()
        .and_then(|value| AgentMode::parse(&value))
        .unwrap_or_default();
    registry.set_mode(initial_mode);
    let mut app = App {
        workspace: workspace_path,
        input: InputBuffer::new(),
        entries,
        status: initial_status,
        busy: false,
        agent_phase: AgentPhase::Idle,
        model_phase: ModelPhase::Idle,
        thinking_summary: String::new(),
        usage: Usage::default(),
        context_used_tokens: estimate_context_tokens(&conversation),
        context_limit_tokens: config.provider.resolved_context_window_tokens(),
        context_meter_enabled: config.ui.context_meter,
        pending_approval: None,
        settings: None,
        palette: None,
        mode: initial_mode,
        leader_pending: false,
        expanded_tools: HashSet::new(),
        file_suggestions: Vec::new(),
        file_selected: 0,
        message_scroll: 0,
        follow_output: true,
        output_scroll_top: None,
        output_selection: None,
        message_layout: None,
        edge_scroll: EdgeScroll::default(),
        session_id,
        sessions,
        conversation,
        storage,
        config,
        registry,
        runner,
        active_secret,
        agent_tx,
        agent_rx,
        active_task: None,
        should_quit: false,
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    ) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    if let Err(error) = terminal.clear() {
        let _ = disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        return Err(error.into());
    }

    let result = event_loop(&mut terminal, &mut app).await;

    let raw_mode_result = disable_raw_mode();
    let screen_result = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let cursor_result = terminal.show_cursor();
    result?;
    raw_mode_result?;
    screen_result?;
    cursor_result?;
    Ok(())
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut edge_scroll_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    terminal.draw(|frame| ui::draw(frame, app))?;

    while !app.should_quit {
        if app
            .output_selection
            .is_some_and(|selection| selection.dragging)
            && app.edge_scroll.direction != 0
            && edge_scroll_timer.is_none()
        {
            edge_scroll_timer = Some(Box::pin(tokio::time::sleep(
                std::time::Duration::from_millis(80),
            )));
        }
        if !app
            .output_selection
            .is_some_and(|selection| selection.dragging)
            || app.edge_scroll.direction == 0
        {
            edge_scroll_timer = None;
        }
        let edge_scroll_tick = async {
            if let Some(timer) = edge_scroll_timer.as_mut() {
                timer.await;
            } else {
                pending::<()>().await;
            }
        };
        let mut did_edge_scroll = false;
        tokio::select! {
            _ = edge_scroll_tick => {
                did_edge_scroll = true;
            }
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(event)) => {
                        if let Some(sequence) = handle_terminal_event(app, event).await? {
                            execute!(terminal.backend_mut(), Print(sequence))?;
                        }
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => break,
                }
            }
            agent_event = app.agent_rx.recv() => {
                if let Some(event) = agent_event {
                    handle_agent_event(app, event);
                }
            }
        }
        if did_edge_scroll {
            edge_scroll_timer = None;
            auto_scroll_selection(app);
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
    }
    if let Some(task) = app.active_task.take() {
        task.abort();
    }
    if let Some(approval) = app.pending_approval.take() {
        if let ApprovalAction::Agent(reply) = approval.action {
            let _ = reply.send(false);
        }
    }
    Ok(())
}

async fn handle_terminal_event(app: &mut App, event: Event) -> Result<Option<String>> {
    if let Event::Paste(text) = &event {
        if !app.busy && app.settings.is_none() && app.palette.is_none() {
            app.input.insert_str(text);
            update_file_suggestions(app);
        }
        return Ok(None);
    }
    if let Event::Mouse(mouse) = event {
        if output_mouse_event_allowed(
            mouse.kind,
            app.settings.is_some(),
            app.palette.is_some(),
            app.pending_approval.is_some(),
        ) {
            return Ok(handle_output_mouse(app, mouse));
        }
        return Ok(None);
    }
    let Event::Key(key) = event else {
        return Ok(None);
    };
    if key.kind != KeyEventKind::Press {
        return Ok(None);
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return Ok(None);
    }
    if app.settings.is_some() {
        handle_settings_key(app, key.code, key.modifiers);
        return Ok(None);
    }
    if app.palette.is_some() {
        handle_palette_key(app, key.code, key.modifiers);
        return Ok(None);
    }
    if app.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => resolve_approval(app, true),
            KeyCode::Char('n') | KeyCode::Char('N') => resolve_approval(app, false),
            KeyCode::Esc => cancel_active_request(app),
            _ => {}
        }
        return Ok(None);
    }
    match key.code {
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            app.palette = Some(CommandPaletteState {
                query: String::new(),
                selected: 0,
            });
            app.status = "命令面板 | 输入筛选 | Enter 执行 | Esc 关闭".into();
        }
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            if let Some(index) = app
                .entries
                .iter()
                .rposition(|entry| matches!(entry.kind, DisplayKind::Tool))
            {
                app.clear_output_selection();
                if !app.expanded_tools.insert(index) {
                    app.expanded_tools.remove(&index);
                }
                app.status = if app.expanded_tools.contains(&index) {
                    "工具详情已展开".into()
                } else {
                    "工具详情已折叠".into()
                };
            }
        }
        KeyCode::PageUp if !app.busy => scroll_messages(app, 5),
        KeyCode::PageDown if !app.busy => scroll_messages(app, -5),
        KeyCode::Up if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            scroll_messages(app, 3)
        }
        KeyCode::Down if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            scroll_messages(app, -3)
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            scroll_to_bottom(app)
        }
        KeyCode::PageUp if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            scroll_messages(app, 5)
        }
        KeyCode::PageDown if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            scroll_messages(app, -5)
        }
        KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            app.leader_pending = true;
            app.status = "快捷键：n 新建 | s 设置 | f 分支 | p 面板 | q 退出".into();
        }
        _ if app.leader_pending => {
            app.leader_pending = false;
            match key.code {
                KeyCode::Char('n') => create_session(app)?,
                KeyCode::Char('s') => open_settings(app),
                KeyCode::Char('f') => execute_command(app, Command::Fork)?,
                KeyCode::Char('p') => {
                    app.palette = Some(CommandPaletteState {
                        query: String::new(),
                        selected: 0,
                    });
                }
                KeyCode::Char('q') => app.should_quit = true,
                _ => app.status = "未知快捷键".into(),
            }
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            open_settings(app);
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            create_session(app)?;
        }
        KeyCode::Up
            if !app.busy
                && key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            switch_session(app, -1)?;
        }
        KeyCode::Down
            if !app.busy
                && key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            switch_session(app, 1)?;
        }
        KeyCode::Esc => {
            app.leader_pending = false;
            if let Some(task) = app.active_task.take() {
                task.abort();
                app.busy = false;
                app.agent_phase = AgentPhase::Idle;
                app.model_phase = ModelPhase::Idle;
                app.status = "已取消".into();
                app.push_entry(DisplayEntry {
                    kind: DisplayKind::System,
                    content: DisplayContent::Markdown("请求已取消。".into()),
                });
            }
        }
        KeyCode::Tab if !app.busy && !app.file_suggestions.is_empty() => {
            apply_file_completion(app);
        }
        KeyCode::Enter if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.insert('\n');
            app.file_suggestions.clear();
        }
        KeyCode::Char('j') if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.insert('\n');
            app.file_suggestions.clear();
        }
        KeyCode::Enter if !app.busy => submit_input(app)?,
        KeyCode::Backspace if !app.busy => {
            app.input.backspace();
            update_file_suggestions(app);
        }
        KeyCode::Delete if !app.busy => {
            app.input.delete();
            update_file_suggestions(app);
        }
        KeyCode::Left if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.select_left();
        }
        KeyCode::Right if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.select_right();
        }
        KeyCode::Char('a') if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.select_all();
        }
        KeyCode::Left if !app.busy => app.input.move_left(),
        KeyCode::Right if !app.busy => app.input.move_right(),
        KeyCode::Home if !app.busy => app.input.move_home(),
        KeyCode::End if !app.busy => app.input.move_end(),
        KeyCode::Up
            if !app.busy && key.modifiers.is_empty() && !app.file_suggestions.is_empty() =>
        {
            app.file_selected = app.file_selected.saturating_sub(1);
        }
        KeyCode::Down
            if !app.busy && key.modifiers.is_empty() && !app.file_suggestions.is_empty() =>
        {
            app.file_selected =
                (app.file_selected + 1).min(app.file_suggestions.len().saturating_sub(1));
        }
        KeyCode::Up if !app.busy && key.modifiers.is_empty() => {
            if app.input.is_empty() {
                app.input.history_previous();
            } else {
                app.input.move_up();
            }
        }
        KeyCode::Down if !app.busy && key.modifiers.is_empty() => {
            if app.input.is_empty() {
                app.input.history_next();
            } else {
                app.input.move_down();
            }
        }
        KeyCode::Char('w') if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_word_left();
        }
        KeyCode::Char('u') if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.clear();
        }
        KeyCode::Char(character)
            if !app.busy
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.input.insert(character);
            update_file_suggestions(app);
        }
        _ => {}
    }
    Ok(None)
}

fn handle_output_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) -> Option<String> {
    match mouse.kind {
        MouseEventKind::ScrollUp => scroll_messages(app, 5),
        MouseEventKind::ScrollDown => scroll_messages(app, -5),
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(offset) = app
                .message_layout
                .as_ref()
                .and_then(|layout| layout.hit_test(mouse.column, mouse.row))
            else {
                app.clear_output_selection();
                return None;
            };
            if app.follow_output {
                app.output_scroll_top = app.message_layout.as_ref().map(|layout| layout.scroll);
                if let Some(layout) = &app.message_layout {
                    app.message_scroll = layout.max_scroll().saturating_sub(layout.scroll);
                }
            }
            app.follow_output = false;
            app.output_selection = Some(OutputSelection::new(offset));
            update_edge_scroll(app, mouse.column, mouse.row);
        }
        MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
            if app
                .output_selection
                .is_some_and(|selection| selection.dragging)
            {
                update_drag_position(app, mouse.column, mouse.row);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.edge_scroll = EdgeScroll::default();
            let mut selection = app.output_selection?;
            selection.dragging = false;
            let Some((start, end)) = selection.range() else {
                app.output_selection = None;
                return None;
            };
            app.output_selection = Some(selection);
            let Some(text) = app
                .message_layout
                .as_ref()
                .and_then(|layout| layout.text.get(start..end))
                .map(str::to_owned)
            else {
                app.status = "复制失败：选区位置已失效".into();
                return None;
            };
            return match crate::clipboard::copy_text(&text) {
                crate::clipboard::CopyResult::Native => {
                    app.status = "系统剪贴板已复制".into();
                    None
                }
                crate::clipboard::CopyResult::Osc52Requested(sequence) => {
                    app.status = "已向终端发送复制请求".into();
                    Some(sequence)
                }
                crate::clipboard::CopyResult::Error(error) => {
                    app.status = format!("复制失败：{error}");
                    None
                }
            };
        }
        _ => {}
    }
    None
}

fn update_drag_position(app: &mut App, column: u16, row: u16) {
    let Some(offset) = app.message_layout.as_ref().and_then(|layout| {
        let clamped_row = row
            .max(layout.viewport.y)
            .min(layout.viewport.bottom().saturating_sub(1));
        layout.hit_test(column, clamped_row)
    }) else {
        return;
    };
    update_edge_scroll(app, column, row);
    if let Some(selection) = &mut app.output_selection {
        selection.active = offset;
    }
}

fn update_edge_scroll(app: &mut App, column: u16, row: u16) {
    let Some(layout) = &app.message_layout else {
        return;
    };
    let direction = if row < layout.viewport.y {
        -1
    } else if row >= layout.viewport.bottom() {
        1
    } else {
        0
    };
    app.edge_scroll = EdgeScroll { direction, column };
}

fn auto_scroll_selection(app: &mut App) {
    let direction = app.edge_scroll.direction;
    if direction == 0
        || !app
            .output_selection
            .is_some_and(|selection| selection.dragging)
    {
        return;
    }
    scroll_messages(app, if direction < 0 { 1 } else { -1 });
    let Some(layout) = &app.message_layout else {
        return;
    };
    let scroll = app.output_scroll_top.unwrap_or(layout.scroll);
    let row = if direction < 0 {
        scroll
    } else {
        scroll.saturating_add(layout.viewport.height.saturating_sub(1) as usize)
    };
    let column = relative_output_column(app.edge_scroll.column, layout.viewport);
    if let Some(offset) = layout.position_at_visual_row(row, column)
        && let Some(selection) = &mut app.output_selection
    {
        selection.active = offset;
    }
}

fn output_mouse_event_allowed(
    kind: MouseEventKind,
    settings_open: bool,
    palette_open: bool,
    approval_open: bool,
) -> bool {
    !settings_open
        && !palette_open
        && !approval_open
        && matches!(
            kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
                | MouseEventKind::Moved
        )
}

fn relative_output_column(column: u16, viewport: ratatui::layout::Rect) -> usize {
    column.saturating_sub(viewport.x) as usize
}

fn clear_output_selection(app: &mut App) {
    app.output_selection = None;
    app.edge_scroll = EdgeScroll::default();
}

fn push_entry(app: &mut App, entry: DisplayEntry) {
    clear_output_selection(app);
    app.entries.push(entry);
}

impl App {
    fn clear_output_selection(&mut self) {
        clear_output_selection(self);
    }

    fn push_entry(&mut self, entry: DisplayEntry) {
        push_entry(self, entry);
    }
}

fn cancel_active_request(app: &mut App) {
    if let Some(approval) = app.pending_approval.take()
        && let ApprovalAction::Agent(reply) = approval.action
    {
        let _ = reply.send(false);
    }
    if let Some(task) = app.active_task.take() {
        task.abort();
    }
    app.busy = false;
    app.agent_phase = AgentPhase::Idle;
    app.model_phase = ModelPhase::Idle;
    app.status = "已取消当前请求".into();
    app.push_entry(DisplayEntry {
        kind: DisplayKind::System,
        content: DisplayContent::Markdown("当前请求已取消。".into()),
    });
}

fn scroll_messages(app: &mut App, delta: isize) {
    let Some(layout) = &app.message_layout else {
        if delta > 0 {
            app.message_scroll = app.message_scroll.saturating_add(delta as usize);
            app.follow_output = false;
        } else {
            app.message_scroll = app.message_scroll.saturating_sub(delta.unsigned_abs());
            if app.message_scroll == 0 {
                app.follow_output = true;
            }
        }
        return;
    };
    let max_scroll = layout.max_scroll();
    let current = app
        .output_scroll_top
        .unwrap_or(layout.scroll)
        .min(max_scroll);
    let next = if delta > 0 {
        current.saturating_add(delta as usize).min(max_scroll)
    } else {
        current.saturating_sub(delta.unsigned_abs())
    };
    if delta < 0 && next == max_scroll {
        app.output_scroll_top = None;
        app.follow_output = true;
        app.message_scroll = 0;
    } else {
        app.output_scroll_top = Some(next);
        app.follow_output = false;
        app.message_scroll = max_scroll.saturating_sub(next);
    }
}

fn scroll_to_bottom(app: &mut App) {
    app.message_scroll = 0;
    app.follow_output = true;
    app.output_scroll_top = None;
}

fn submit_input(app: &mut App) -> Result<()> {
    let input = app.input.as_str().trim().to_owned();
    if input.is_empty() {
        return Ok(());
    }
    app.input.push_history();
    if let Some(command) = input
        .strip_prefix('!')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        app.input.clear();
        return request_shell_approval(app, command.to_owned());
    }
    if input.starts_with('/') {
        if let Some(command) = commands::parse(&input) {
            app.input.clear();
            return execute_command(app, command);
        }
        if let Some(prompt) = expand_custom_command(app, &input) {
            app.input.set(prompt);
            return submit_input(app);
        }
        app.input.clear();
        app.push_entry(DisplayEntry {
            kind: DisplayKind::Error,
            content: DisplayContent::Markdown(format!("未知命令，请使用 /help 查看命令：{input}")),
        });
        return Ok(());
    }
    let Some(runner) = app.runner.clone() else {
        app.status = "请打开提供商设置配置 API Key".into();
        return Ok(());
    };
    app.input.clear();
    app.file_suggestions.clear();
    app.clear_output_selection();
    app.message_scroll = 0;
    app.follow_output = true;
    app.output_scroll_top = None;
    app.push_entry(DisplayEntry {
        kind: DisplayKind::User,
        content: DisplayContent::Markdown(input.clone()),
    });
    app.conversation.push(ConversationItem::Message {
        role: Role::User,
        content: input.clone(),
    });
    app.storage
        .append_message(&app.session_id, Role::User, &input)?;
    for (label, content) in collect_file_context(app, &input) {
        app.conversation.push(ConversationItem::Context {
            label: label.clone(),
            content: content.clone(),
        });
        app.storage
            .append_context(&app.session_id, &label, &content)?;
        app.push_entry(DisplayEntry {
            kind: DisplayKind::System,
            content: DisplayContent::Markdown(format!("已附加文件 @{label}")),
        });
    }
    refresh_sessions(app)?;
    trim_conversation(&mut app.conversation);
    app.context_used_tokens = estimate_context_tokens(&app.conversation);
    app.busy = true;
    app.agent_phase = AgentPhase::Thinking;
    app.model_phase = ModelPhase::Idle;
    app.thinking_summary.clear();
    app.status = "准备请求中…… | Esc 取消".into();
    let items = app.conversation.clone();
    let events = app.agent_tx.clone();
    app.active_task = Some(tokio::spawn(async move {
        runner.run(items, events).await;
    }));
    trim_app_entries(app);
    Ok(())
}

fn expand_custom_command(app: &App, input: &str) -> Option<String> {
    let mut parts = input[1..].trim().splitn(2, char::is_whitespace);
    let name = parts.next()?;
    let arguments = parts.next().unwrap_or("").trim();
    let command = app
        .config
        .commands
        .iter()
        .find(|command| command.name == name)?;
    if command.template.trim().is_empty() {
        return None;
    }
    Some(
        command
            .template
            .replace("{args}", arguments)
            .replace("{workspace}", &app.workspace.display().to_string()),
    )
}

fn collect_file_context(app: &App, input: &str) -> Vec<(String, String)> {
    let mut contexts = Vec::new();
    let mut total = 0usize;
    for token in input
        .split_whitespace()
        .filter_map(|token| token.strip_prefix('@'))
    {
        let path = token.trim_matches(|character: char| {
            matches!(character, ',' | '.' | ':' | ';' | ')' | ']' | '}')
        });
        if path.is_empty() || contexts.iter().any(|(label, _)| label == path) {
            continue;
        }
        let Ok(resolved) = app.registry.workspace().resolve_existing(path) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&resolved) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&resolved) else {
            continue;
        };
        let remaining = (256 * 1024usize).saturating_sub(total);
        if remaining == 0 {
            break;
        }
        let mut content = content;
        if content.len() > remaining {
            content.truncate(remaining);
            while !content.is_char_boundary(content.len()) {
                content.pop();
            }
            content.push_str("\n[context truncated]");
        }
        total = total.saturating_add(content.len());
        contexts.push((path.to_owned(), content));
    }
    contexts
}

fn update_file_suggestions(app: &mut App) {
    app.file_suggestions.clear();
    app.file_selected = 0;
    let token = app.input.as_str().split_whitespace().last().unwrap_or("");
    let Some(query) = token.strip_prefix('@') else {
        return;
    };
    if query.contains('/') && query.ends_with('/') {
        // Directory-specific completion is handled by the same bounded walk;
        // retaining the slash keeps the suggestion easy to insert.
    }
    let mut candidates = WalkBuilder::new(app.registry.workspace().root())
        .hidden(false)
        .standard_filters(true)
        .max_depth(Some(5))
        .build()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry
                .path()
                .strip_prefix(app.registry.workspace().root())
                .ok()?;
            if path.as_os_str().is_empty() || path == std::path::Path::new(".git") {
                return None;
            }
            let value = path.to_string_lossy().replace('\\', "/");
            let score = commands::fuzzy_score(query, &value)?;
            Some((score, value))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(score, value)| (*score, value.len()));
    app.file_suggestions = candidates
        .into_iter()
        .take(10)
        .map(|(_, value)| value)
        .collect();
}

fn apply_file_completion(app: &mut App) {
    let Some(path) = app.file_suggestions.get(app.file_selected).cloned() else {
        return;
    };
    let input = app.input.as_str().to_owned();
    let start = input
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    if !input[start..].starts_with('@') {
        return;
    }
    app.input.set(format!("{}@{} ", &input[..start], path));
    app.file_suggestions.clear();
}

fn open_settings(app: &mut App) {
    app.settings = Some(SettingsState {
        provider: app.config.provider.clone(),
        api_key: String::new(),
        has_existing_key: app
            .active_secret
            .as_ref()
            .is_some_and(|(preset, _)| *preset == app.config.provider.preset),
        field: SettingsField::Preset,
    });
    app.status = "提供商设置".into();
}

fn handle_palette_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Some(palette) = &mut app.palette else {
        return;
    };
    let results = commands::matches(&palette.query, 10);
    match code {
        KeyCode::Esc => {
            app.palette = None;
            app.status = "就绪".into();
        }
        KeyCode::Enter => {
            let selected = results.get(palette.selected).copied();
            let command =
                selected.and_then(|item| commands::parse(commands::COMMAND_NAMES[item.index]));
            app.palette = None;
            if let Some(command) = command {
                if let Err(error) = execute_command(app, command) {
                    app.status = format!("命令失败：{error}");
                }
            }
        }
        KeyCode::Up => {
            palette.selected = palette.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            palette.selected = (palette.selected + 1).min(results.len().saturating_sub(1));
        }
        KeyCode::Backspace => {
            palette.query.pop();
            palette.selected = 0;
        }
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::CONTROL) => {
            palette.query.push(character);
            palette.selected = 0;
        }
        _ => {}
    }
}

fn execute_command(app: &mut App, command: Command) -> Result<()> {
    match command {
        Command::Help => {
            app.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(
                    "## 命令\n\n`/new` `/sessions` `/rename` `/fork` `/delete`\n`/undo` `/redo` `/compact` `/export` `/diff`\n`/plan` `/build` `/explore` `/model` `/provider`\n\nCtrl+P 命令面板 | Ctrl+X 快捷键 | @ 文件 | ! Shell"
                        .into(),
                ),
            });
            app.status = "命令帮助".into();
        }
        Command::NewSession => create_session(app)?,
        Command::Sessions => {
            let listing = app
                .sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    let marker = if session.id == app.session_id {
                        "*"
                    } else {
                        " "
                    };
                    format!("{marker} {}. {}", index + 1, session.title)
                })
                .collect::<Vec<_>>()
                .join("\n");
            app.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(if listing.is_empty() {
                    "暂无会话".into()
                } else {
                    format!("## 会话\n\n{listing}")
                }),
            });
        }
        Command::Provider => open_settings(app),
        Command::Model(model) => {
            if let Some(model) = model {
                if model.trim().is_empty() {
                    app.status = "模型不能为空".into();
                } else {
                    app.config.provider.model = model.trim().to_owned();
                    app.context_limit_tokens = app.config.provider.resolved_context_window_tokens();
                    rebuild_runner(app)?;
                    app.status = format!("模型已设置为 {}", app.config.provider.model);
                    let _ = app.config.save();
                }
            } else {
                app.status = format!("当前模型：{}", app.config.provider.model);
            }
        }
        Command::Agent(agent) => {
            if let Some(name) = agent {
                if let Some(configured) = app.config.agents.iter().find(|item| item.name == name) {
                    app.mode = configured.mode;
                    app.registry.set_mode(app.mode);
                    app.storage
                        .set_session_mode(&app.session_id, app.mode.as_str())?;
                    // Force a fresh provider context so the new mode contract is
                    // sent as the stable system prefix on the next request.
                    app.storage.clear_response_id(&app.session_id)?;
                    app.status = format!("Agent：{} | 模式：{}", name, app.mode);
                    app.push_entry(DisplayEntry {
                        kind: DisplayKind::System,
                        content: DisplayContent::Markdown(format!(
                            "Agent 模式已切换为 **{}**。下一次模型请求将使用 {} 执行约束。",
                            app.mode.as_str().to_ascii_uppercase(),
                            app.mode.as_str()
                        )),
                    });
                } else {
                    app.status = format!("未知 Agent：{name}");
                }
            } else {
                app.status = format!("当前 Agent 模式：{}", app.mode);
            }
        }
        Command::Mode(mode) => {
            app.mode = mode;
            app.registry.set_mode(mode);
            let _ = app.storage.set_session_mode(&app.session_id, mode.as_str());
            app.storage.clear_response_id(&app.session_id)?;
            app.status = format!("模式已切换为 {}", mode.as_str().to_ascii_uppercase());
            app.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(format!(
                    "Agent 模式已切换为 **{}**。下一次模型请求将使用 {} 执行约束。",
                    mode.as_str().to_ascii_uppercase(),
                    mode.as_str()
                )),
            });
        }
        Command::Clear => {
            app.entries.clear();
            app.clear_output_selection();
            app.status = "显示已清空，会话历史仍保留".into();
        }
        Command::Quit => app.should_quit = true,
        Command::Rename(title) => {
            let title = title.unwrap_or_else(|| "Untitled session".into());
            app.storage.rename_session(&app.session_id, &title)?;
            refresh_sessions(app)?;
            app.status = format!("会话已重命名为 {}", title.trim());
        }
        Command::Delete => {
            if app.sessions.len() <= 1 {
                app.status = "不能删除最后一个会话，请先执行 /new".into();
            } else {
                let deleted = app.session_id.clone();
                app.storage.delete_session(&deleted)?;
                let next = app
                    .storage
                    .latest_session(&app.workspace)?
                    .context("no session remains after delete")?;
                activate_session(app, next)?;
                refresh_sessions(app)?;
                app.status = "会话已删除".into();
            }
        }
        Command::Fork => {
            let fork = app.storage.fork_session(&app.session_id)?;
            activate_session(app, fork)?;
            refresh_sessions(app)?;
            app.status = "会话已创建分支".into();
        }
        Command::Undo => {
            if app.storage.undo(&app.session_id)? {
                app.storage.clear_response_id(&app.session_id)?;
                let session_id = app.session_id.clone();
                activate_session(app, session_id)?;
                app.status = "已撤销上一轮".into();
            } else {
                app.status = "没有可撤销的内容".into();
            }
        }
        Command::Redo => {
            if app.storage.redo(&app.session_id)? {
                app.storage.clear_response_id(&app.session_id)?;
                let session_id = app.session_id.clone();
                activate_session(app, session_id)?;
                app.status = "已重做上一轮".into();
            } else {
                app.status = "没有可重做的内容".into();
            }
        }
        Command::Compact => {
            let hidden = app.storage.compact_session(&app.session_id, 8)?;
            if hidden > 0 {
                app.storage.append_message(
                    &app.session_id,
                    Role::System,
                    &format!("Local compaction hid {hidden} older messages."),
                )?;
            }
            let session_id = app.session_id.clone();
            activate_session(app, session_id)?;
            app.status = format!("已压缩 {hidden} 条旧消息");
        }
        Command::Export(path) => export_session(app, path)?,
        Command::Diff => start_diff(app)?,
    }
    Ok(())
}

fn export_session(app: &mut App, requested: Option<String>) -> Result<()> {
    let filename = requested.unwrap_or_else(|| format!(".1h-agent-{}.md", app.session_id));
    let target = app
        .registry
        .workspace()
        .resolve_new(&filename)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let mut output = String::new();
    for item in &app.conversation {
        if let ConversationItem::Message { role, content } = item {
            let label = match role {
                Role::System => "System",
                Role::User => "You",
                Role::Assistant => "Agent",
            };
            output.push_str(&format!("## {label}\n\n{content}\n\n"));
        }
        if output.len() > 5 * 1024 * 1024 {
            output.push_str("\n[export truncated]\n");
            break;
        }
    }
    std::fs::write(&target, output)
        .with_context(|| format!("cannot write export {}", target.display()))?;
    app.status = format!("对话已导出到 {}", target.display());
    Ok(())
}

fn rebuild_runner(app: &mut App) -> Result<()> {
    let Some((_, api_key)) = &app.active_secret else {
        app.runner = None;
        return Ok(());
    };
    let provider = OpenAiClient::new(app.config.provider.base_url.clone(), api_key.clone())?;
    app.runner = Some(AgentRunner::new(
        provider,
        app.config.provider.clone(),
        app.config.runtime.clone(),
        app.registry.clone(),
        app.storage.clone(),
        app.session_id.clone(),
    ));
    Ok(())
}

fn start_diff(app: &mut App) -> Result<()> {
    if app.busy {
        return Ok(());
    }
    let registry = app.registry.clone();
    let events = app.agent_tx.clone();
    app.busy = true;
    app.status = "正在收集 Git diff…… | Esc 取消".into();
    app.active_task = Some(tokio::spawn(async move {
        let call = ToolCall {
            id: format!("diff_{}", uuid::Uuid::new_v4()),
            name: "git".into(),
            arguments: serde_json::json!({"args":["diff","--no-ext-diff","--unified=3"]}),
        };
        let result = registry
            .execute(&call)
            .await
            .unwrap_or_else(|error| error.to_string());
        let _ = events
            .send(AgentEvent::LocalCommandFinished {
                command: "/diff".into(),
                result,
            })
            .await;
    }));
    Ok(())
}

fn handle_settings_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            app.settings = None;
            app.status = "设置已取消".into();
        }
        KeyCode::Tab => {
            if let Some(settings) = &mut app.settings {
                settings.field = settings.field.cycle(1);
            }
        }
        KeyCode::BackTab => {
            if let Some(settings) = &mut app.settings {
                settings.field = settings.field.cycle(-1);
            }
        }
        KeyCode::Up => {
            if let Some(settings) = &mut app.settings {
                settings.field = settings.field.cycle(-1);
            }
        }
        KeyCode::Down => {
            if let Some(settings) = &mut app.settings {
                settings.field = settings.field.cycle(1);
            }
        }
        KeyCode::Left => cycle_setting_value(app, -1),
        KeyCode::Right => cycle_setting_value(app, 1),
        KeyCode::Backspace => edit_setting(app, None),
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::CONTROL) => {
            edit_setting(app, Some(character));
        }
        KeyCode::Enter => {
            if let Err(error) = apply_settings(app) {
                app.status = format!("设置错误：{}", secrets::redact(&error.to_string()));
            }
        }
        _ => {}
    }
}

fn cycle_setting_value(app: &mut App, direction: i32) {
    let Some(settings) = &mut app.settings else {
        return;
    };
    match settings.field {
        SettingsField::Preset => {
            let current = ProviderPreset::ALL
                .iter()
                .position(|preset| *preset == settings.provider.preset)
                .unwrap_or(0) as i32;
            let next = (current + direction).rem_euclid(ProviderPreset::ALL.len() as i32) as usize;
            settings.provider = ProviderPreset::ALL[next].defaults();
            settings.api_key.clear();
            settings.has_existing_key = app
                .active_secret
                .as_ref()
                .is_some_and(|(preset, _)| *preset == settings.provider.preset);
        }
        SettingsField::Protocol if settings.provider.preset.supports_responses() => {
            settings.provider.kind = match settings.provider.kind {
                ProviderKind::ChatCompletions => ProviderKind::Responses,
                ProviderKind::Responses => ProviderKind::ChatCompletions,
            };
        }
        _ => {}
    }
}

fn edit_setting(app: &mut App, character: Option<char>) {
    let Some(settings) = &mut app.settings else {
        return;
    };
    let value = match settings.field {
        SettingsField::BaseUrl => &mut settings.provider.base_url,
        SettingsField::Model => &mut settings.provider.model,
        SettingsField::ApiKey => &mut settings.api_key,
        _ => return,
    };
    match character {
        Some(character) => value.push(character),
        None => {
            value.pop();
        }
    }
}

fn apply_settings(app: &mut App) -> Result<()> {
    let settings = app.settings.as_ref().context("settings are not open")?;
    let mut provider_config = settings.provider.clone();
    provider_config.validate()?;
    let entered_key = settings.api_key.trim();
    let api_key = if !entered_key.is_empty() {
        entered_key.to_owned()
    } else if let Some((preset, key)) = &app.active_secret {
        if *preset == provider_config.preset {
            key.clone()
        } else {
            secrets::api_key(provider_config.preset)?
        }
    } else {
        secrets::api_key(provider_config.preset)?
    };

    let provider = OpenAiClient::new(provider_config.base_url.clone(), api_key.clone())?;
    app.runner = Some(AgentRunner::new(
        provider,
        provider_config.clone(),
        app.config.runtime.clone(),
        app.registry.clone(),
        app.storage.clone(),
        app.session_id.clone(),
    ));
    app.active_secret = Some((provider_config.preset, api_key));
    app.config.provider = provider_config.clone();
    app.context_limit_tokens = provider_config.resolved_context_window_tokens();

    let key_warning = if !entered_key.is_empty() {
        secrets::store_api_key(provider_config.preset, entered_key)
            .err()
            .map(|error| {
                format!(
                    "API Key 仅本次运行有效：{}",
                    secrets::redact(&error.to_string())
                )
            })
    } else {
        None
    };
    let config_warning = app.config.save().err().map(|error| {
        format!(
            "配置仅本次运行有效：{}",
            secrets::redact(&error.to_string())
        )
    });
    let warnings = [key_warning, config_warning]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    app.settings = None;
    app.status = format!(
        "就绪 | {} | {}{}",
        provider_config.preset.label(),
        provider_config.model,
        if warnings.is_empty() {
            String::new()
        } else {
            format!(" | {warnings}")
        }
    );
    Ok(())
}

fn handle_agent_event(app: &mut App, event: AgentEvent) {
    match event {
        AgentEvent::ThinkingSummary(summary) => {
            const MAX_SUMMARY_BYTES: usize = 1024;
            app.agent_phase = AgentPhase::Thinking;
            app.model_phase = ModelPhase::Idle;
            app.thinking_summary = truncate_text(&summary, MAX_SUMMARY_BYTES);
            app.push_entry(DisplayEntry {
                kind: DisplayKind::Thinking,
                content: DisplayContent::Markdown(app.thinking_summary.clone()),
            });
            app.status = "正在准备下一步模型请求".into();
        }
        AgentEvent::ModelStreaming => {
            app.model_phase = ModelPhase::Streaming;
            app.status = "等待模型流式响应".into();
        }
        AgentEvent::WebSearchStarted { query } => {
            app.agent_phase = AgentPhase::ToolRunning;
            app.model_phase = ModelPhase::Streaming;
            let already_open = app.entries.last().is_some_and(|entry| {
                matches!(
                    &entry.content,
                    DisplayContent::ToolCall { name, .. } if name == "web_search"
                )
            });
            if !already_open {
                app.push_entry(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::ToolCall {
                        name: "web_search".into(),
                        arguments: serde_json::json!({"query": query}),
                    },
                });
            }
            app.status = "正在联网搜索".into();
        }
        AgentEvent::WebSearchResult {
            title,
            url,
            snippet,
        } => {
            let context = format!("{url}\n{snippet}");
            app.push_entry(DisplayEntry {
                kind: DisplayKind::Tool,
                content: DisplayContent::ToolResult {
                    name: format!("web_search: {title}"),
                    result: context.clone(),
                },
            });
        }
        AgentEvent::WebSearchCompleted { count } => {
            app.agent_phase = AgentPhase::Thinking;
            app.status = if count == 0 {
                "联网搜索完成".into()
            } else {
                format!("联网搜索完成：{count} 条结果")
            };
        }
        AgentEvent::Cancelled(reason) => {
            app.busy = false;
            app.active_task = None;
            app.pending_approval = None;
            app.agent_phase = AgentPhase::Idle;
            app.model_phase = ModelPhase::Idle;
            app.status = if reason.contains("approval") {
                "审批等待已取消".into()
            } else {
                "请求已取消".into()
            };
        }
        AgentEvent::TextDelta(delta) => {
            app.agent_phase = AgentPhase::StreamingText;
            app.model_phase = ModelPhase::Streaming;
            if let Some(entry) = app.entries.last_mut()
                && matches!(entry.kind, DisplayKind::Assistant)
                && let DisplayContent::Markdown(text) = &mut entry.content
            {
                text.push_str(&delta);
            } else {
                app.push_entry(DisplayEntry {
                    kind: DisplayKind::Assistant,
                    content: DisplayContent::Markdown(delta),
                });
            }
            app.status = "正在输出正文…… | Esc 取消".into();
        }
        AgentEvent::Approval {
            call,
            reason,
            reply,
        } => {
            app.agent_phase = AgentPhase::WaitingApproval;
            app.model_phase = ModelPhase::Idle;
            app.status = "需要确认工具权限".into();
            app.pending_approval = Some(PendingApproval {
                call,
                reason,
                action: ApprovalAction::Agent(reply),
                created_at: Instant::now(),
            });
        }
        AgentEvent::ToolStarted(call) => {
            app.agent_phase = AgentPhase::ToolRunning;
            app.model_phase = ModelPhase::Idle;
            app.status = format!("正在执行 {}……", call.name);
            app.push_entry(DisplayEntry {
                kind: DisplayKind::Tool,
                content: DisplayContent::ToolCall {
                    name: call.name,
                    arguments: call.arguments,
                },
            });
        }
        AgentEvent::ToolFinished { call, result } => {
            app.agent_phase = AgentPhase::Thinking;
            app.push_entry(DisplayEntry {
                kind: DisplayKind::Tool,
                content: DisplayContent::ToolResult {
                    name: call.name,
                    result,
                },
            });
            app.status = "正在将工具结果交给模型……".into();
        }
        AgentEvent::Usage(usage) => {
            app.context_used_tokens = usage
                .input_tokens
                .max(estimate_context_tokens(&app.conversation));
            app.usage = usage;
        }
        AgentEvent::Completed { items } => {
            app.conversation = items;
            trim_conversation(&mut app.conversation);
            app.busy = false;
            app.active_task = None;
            app.agent_phase = AgentPhase::Idle;
            app.model_phase = ModelPhase::Completed;
            app.status = if refresh_sessions(app).is_ok() {
                "就绪".into()
            } else {
                "就绪，但刷新会话失败".into()
            };
        }
        AgentEvent::Failed(error) => {
            app.push_entry(DisplayEntry {
                kind: DisplayKind::Error,
                content: DisplayContent::Markdown(secrets::redact(&error)),
            });
            app.busy = false;
            app.active_task = None;
            app.agent_phase = AgentPhase::Failed;
            app.model_phase = ModelPhase::Failed;
            app.status = if error.contains("maximum tool turns") {
                "Agent 已达到最大工具轮数".into()
            } else {
                "请求失败".into()
            };
        }
        AgentEvent::LocalCommandFinished { command, result } => {
            if command == "/diff" {
                app.push_entry(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::Diff(result),
                });
                app.busy = false;
                app.active_task = None;
                app.agent_phase = AgentPhase::Idle;
                app.model_phase = ModelPhase::Completed;
                app.status = "Git diff 已准备好".into();
                trim_app_entries(app);
                return;
            }
            app.push_entry(DisplayEntry {
                kind: DisplayKind::Tool,
                content: DisplayContent::ToolResult {
                    name: format!("! {command}"),
                    result: result.clone(),
                },
            });
            app.conversation.push(ConversationItem::Context {
                label: format!("shell: {command}"),
                content: result.clone(),
            });
            if let Err(error) =
                app.storage
                    .append_context(&app.session_id, &format!("shell: {command}"), &result)
            {
                app.status = format!("命令已完成，但保存失败：{error}");
            } else {
                app.status = "Shell 命令已完成".into();
            }
            app.busy = false;
            app.active_task = None;
            app.agent_phase = AgentPhase::Idle;
            app.model_phase = ModelPhase::Completed;
        }
    }
    trim_app_entries(app);
}

fn resolve_approval(app: &mut App, approved: bool) {
    if let Some(approval) = app.pending_approval.take() {
        match approval.action {
            ApprovalAction::Agent(reply) => {
                let _ = reply.send(approved);
                app.agent_phase = AgentPhase::Thinking;
                app.model_phase = ModelPhase::Idle;
                app.status = if approved {
                    "已批准，开始执行工具……".into()
                } else {
                    "已拒绝，将结果返回模型……".into()
                };
            }
            ApprovalAction::Shell(command) => {
                if approved {
                    let registry = app.registry.clone();
                    let events = app.agent_tx.clone();
                    let command_for_event = command.clone();
                    app.busy = true;
                    app.agent_phase = AgentPhase::ToolRunning;
                    app.model_phase = ModelPhase::Idle;
                    app.status = "正在执行 Shell 命令…… | Esc 取消".into();
                    app.active_task = Some(tokio::spawn(async move {
                        let result = registry
                            .execute_shell(&command)
                            .await
                            .unwrap_or_else(|error| error.to_string());
                        let _ = events
                            .send(AgentEvent::LocalCommandFinished {
                                command: command_for_event,
                                result,
                            })
                            .await;
                    }));
                } else {
                    app.push_entry(DisplayEntry {
                        kind: DisplayKind::System,
                        content: DisplayContent::Markdown("Shell 命令已拒绝。".into()),
                    });
                    app.status = "Shell 命令已拒绝".into();
                    app.agent_phase = AgentPhase::Idle;
                }
            }
        }
    }
}

fn request_shell_approval(app: &mut App, command: String) -> Result<()> {
    let call = ToolCall {
        id: format!("shell_{}", uuid::Uuid::new_v4()),
        name: "terminal_shell".into(),
        arguments: serde_json::json!({ "command": command }),
    };
    app.pending_approval = Some(PendingApproval {
        call,
        reason: "! 命令将通过 workspace Shell 执行".into(),
        action: ApprovalAction::Shell(command),
        created_at: Instant::now(),
    });
    app.agent_phase = AgentPhase::WaitingApproval;
    app.model_phase = ModelPhase::Idle;
    app.status = "Shell 命令需要确认".into();
    Ok(())
}

fn create_session(app: &mut App) -> Result<()> {
    let session_id = app.storage.create_session(&app.workspace)?;
    activate_session(app, session_id)?;
    refresh_sessions(app)?;
    app.status = "新会话已就绪".into();
    Ok(())
}

fn switch_session(app: &mut App, direction: i32) -> Result<()> {
    refresh_sessions(app)?;
    if app.sessions.len() < 2 {
        app.status = "当前只有一个会话 | Ctrl+N 新建会话".into();
        return Ok(());
    }
    let current = app
        .sessions
        .iter()
        .position(|session| session.id == app.session_id)
        .unwrap_or(0) as i32;
    let next = (current + direction).rem_euclid(app.sessions.len() as i32) as usize;
    let session_id = app.sessions[next].id.clone();
    activate_session(app, session_id)
}

fn activate_session(app: &mut App, session_id: String) -> Result<()> {
    let mut conversation = app.storage.load_messages(&session_id)?;
    trim_conversation(&mut conversation);
    let entries = display_entries(&conversation);
    app.mode = app
        .storage
        .session_mode(&session_id)
        .ok()
        .and_then(|value| AgentMode::parse(&value))
        .unwrap_or_default();
    app.registry.set_mode(app.mode);
    let runner = if let Some((_, api_key)) = &app.active_secret {
        let provider = OpenAiClient::new(app.config.provider.base_url.clone(), api_key.clone())?;
        Some(AgentRunner::new(
            provider,
            app.config.provider.clone(),
            app.config.runtime.clone(),
            app.registry.clone(),
            app.storage.clone(),
            session_id.clone(),
        ))
    } else {
        None
    };

    app.session_id = session_id;
    app.conversation = conversation;
    app.entries = entries;
    app.input.clear();
    app.file_suggestions.clear();
    app.expanded_tools.clear();
    app.clear_output_selection();
    app.message_scroll = 0;
    app.follow_output = true;
    app.output_scroll_top = None;
    app.usage = Usage::default();
    app.context_used_tokens = estimate_context_tokens(&app.conversation);
    app.context_limit_tokens = app.config.provider.resolved_context_window_tokens();
    app.agent_phase = AgentPhase::Idle;
    app.model_phase = ModelPhase::Idle;
    app.thinking_summary.clear();
    app.runner = runner;
    app.status = if app.runner.is_some() {
        format!(
            "就绪 | {} | {}",
            app.config.provider.preset.label(),
            app.config.provider.model
        )
    } else {
        "需要配置提供商".into()
    };
    Ok(())
}

fn refresh_sessions(app: &mut App) -> Result<()> {
    app.sessions = app.storage.list_sessions(&app.workspace)?;
    Ok(())
}

fn display_entries(conversation: &[ConversationItem]) -> Vec<DisplayEntry> {
    let mut entries = Vec::new();
    let mut tool_names = HashMap::new();
    for item in conversation {
        match item {
            ConversationItem::Message { role, content } => entries.push(DisplayEntry {
                kind: match role {
                    Role::User => DisplayKind::User,
                    Role::Assistant => DisplayKind::Assistant,
                    Role::System => DisplayKind::System,
                },
                content: DisplayContent::Markdown(content.clone()),
            }),
            ConversationItem::ThinkingSummary { content } => entries.push(DisplayEntry {
                kind: DisplayKind::Thinking,
                content: DisplayContent::Markdown(content.clone()),
            }),
            ConversationItem::Context { label, content } => entries.push(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(format!("### @{label}\n\n{content}")),
            }),
            ConversationItem::ProviderItem { item } => {
                if item.get("type").and_then(serde_json::Value::as_str) == Some("web_search_call") {
                    entries.push(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::ToolCall {
                            name: "DeepSeek web_search".into(),
                            arguments: item.get("action").cloned().unwrap_or_default(),
                        },
                    });
                }
            }
            ConversationItem::AssistantToolCalls { calls } => {
                for call in calls {
                    tool_names.insert(call.id.clone(), call.name.clone());
                    entries.push(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::ToolCall {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        },
                    });
                }
            }
            ConversationItem::ToolOutput { call_id, output } => {
                entries.push(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::ToolResult {
                        name: tool_names
                            .get(call_id)
                            .cloned()
                            .unwrap_or_else(|| "tool".into()),
                        result: output.clone(),
                    },
                });
            }
        }
    }
    if entries.is_empty() {
        entries.push(DisplayEntry {
            kind: DisplayKind::System,
            content: DisplayContent::Markdown("1H-Agent 已就绪，请输入任务并按 Enter。".into()),
        });
    }
    entries
}

fn trim_entries(entries: &mut Vec<DisplayEntry>) {
    const MAX_ENTRIES: usize = 1000;
    const MAX_BYTES: usize = 2 * 1024 * 1024;
    while entries.len() > MAX_ENTRIES || display_entry_bytes(entries) > MAX_BYTES {
        if entries.len() > MAX_ENTRIES {
            let count = entries.len() - MAX_ENTRIES;
            entries.drain(..count);
        } else {
            entries.remove(0);
        }
    }
}

fn trim_app_entries(app: &mut App) {
    let before = app.entries.len();
    trim_entries(&mut app.entries);
    if before != app.entries.len() {
        app.expanded_tools.clear();
        app.clear_output_selection();
    }
}

fn trim_conversation(items: &mut Vec<ConversationItem>) {
    const MAX_ITEMS: usize = 200;
    const MAX_BYTES: usize = 1024 * 1024;
    let mut removed = 0usize;
    while items.len() > MAX_ITEMS || conversation_bytes(items) > MAX_BYTES {
        if items.is_empty() {
            break;
        }
        items.remove(0);
        removed += 1;
    }
    if removed > 0 {
        items.insert(
            0,
            ConversationItem::Message {
                role: Role::System,
                content: format!(
                    "Earlier context was locally compacted ({removed} items omitted)."
                ),
            },
        );
    }
}

fn conversation_bytes(items: &[ConversationItem]) -> usize {
    items
        .iter()
        .map(|item| match item {
            ConversationItem::Message { content, .. }
            | ConversationItem::Context { content, .. } => content.len(),
            ConversationItem::ThinkingSummary { .. } => 0,
            ConversationItem::ProviderItem { item } => item.to_string().len(),
            ConversationItem::AssistantToolCalls { calls } => calls
                .iter()
                .map(|call| call.name.len() + call.arguments.to_string().len())
                .sum(),
            ConversationItem::ToolOutput { output, .. } => output.len(),
        })
        .sum()
}

fn estimate_context_tokens(items: &[ConversationItem]) -> u64 {
    let bytes = conversation_bytes(items) as u64;
    // A conservative, allocation-free estimate for mixed prose/JSON context.
    (bytes.saturating_add(3) / 4).max(1)
}

fn display_entry_bytes(entries: &[DisplayEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match &entry.content {
            DisplayContent::Markdown(value) => value.len(),
            DisplayContent::Diff(value) => value.len(),
            DisplayContent::ToolCall { name, arguments } => {
                name.len() + arguments.to_string().len()
            }
            DisplayContent::ToolResult { name, result } => name.len() + result.len(),
        })
        .sum()
}

fn truncate_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub(3).min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn display_restore_keeps_agent_tool_agent_order() {
        let call = ToolCall {
            id: "call_1".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"src/lib.rs"}),
        };
        let entries = display_entries(&[
            ConversationItem::Message {
                role: Role::Assistant,
                content: "before".into(),
            },
            ConversationItem::AssistantToolCalls { calls: vec![call] },
            ConversationItem::ToolOutput {
                call_id: "call_1".into(),
                output: "ok".into(),
            },
            ConversationItem::Message {
                role: Role::Assistant,
                content: "after".into(),
            },
        ]);
        assert!(matches!(entries[0].kind, DisplayKind::Assistant));
        assert!(matches!(entries[1].kind, DisplayKind::Tool));
        assert!(matches!(entries[2].kind, DisplayKind::Tool));
        assert!(matches!(entries[3].kind, DisplayKind::Assistant));
    }

    #[test]
    fn context_estimate_is_bounded_and_nonzero() {
        assert_eq!(estimate_context_tokens(&[]), 1);
        assert_eq!(
            estimate_context_tokens(&[ConversationItem::Message {
                role: Role::User,
                content: "12345678".into(),
            }]),
            2
        );
    }

    #[test]
    fn edge_scroll_column_is_relative_to_output_viewport() {
        let left_aligned = Rect::new(0, 4, 20, 6);
        let sidebar_offset = Rect::new(30, 4, 20, 6);
        assert_eq!(relative_output_column(5, left_aligned), 5);
        assert_eq!(relative_output_column(35, sidebar_offset), 5);
        assert_eq!(relative_output_column(29, sidebar_offset), 0);
        assert_eq!(relative_output_column(55, sidebar_offset), 25);
    }

    #[test]
    fn modal_popups_ignore_all_output_mouse_events() {
        let events = [
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ];
        for (settings_open, palette_open, approval_open) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            for event in events {
                assert!(!output_mouse_event_allowed(
                    event,
                    settings_open,
                    palette_open,
                    approval_open
                ));
            }
        }
        assert!(output_mouse_event_allowed(
            MouseEventKind::ScrollDown,
            false,
            false,
            false
        ));
    }
}
