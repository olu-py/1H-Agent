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
        Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        MouseButton, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ignore::WalkBuilder;
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    agent::{AgentEvent, AgentRunner},
    commands::{self, AgentMode, Command},
    config::{
        Config, ProviderConfig, ProviderKind, ProviderPreset, ThinkingCapability, ThinkingLevel,
        ThinkingProfile, thinking_profile,
    },
    input::InputBuffer,
    output::{EdgeScroll, InteractionTarget, MessageLayout, OutputSelection},
    provider::{ConversationItem, OpenAiClient, Role, ToolCall, Usage},
    secrets,
    security::Workspace,
    storage::{SessionSummary, Storage},
    tools::ToolRegistry,
    ui,
};

const MOUSE_WHEEL_SCROLL_LINES: isize = 1;

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
    Tool(ToolDisplay),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDisplayStatus {
    Running,
    Completed,
    Failed,
    Rejected,
}

#[derive(Clone, Debug)]
pub struct ToolDisplay {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub status: ToolDisplayStatus,
    pub result: Option<String>,
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
    Thinking,
    ApiKey,
}

impl SettingsField {
    const ALL: [Self; 6] = [
        Self::Preset,
        Self::Protocol,
        Self::BaseUrl,
        Self::Model,
        Self::Thinking,
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
    pub thinking_last_line: String,
    pub thinking_active: bool,
    pub thinking_buffer: String,
    pub thinking_buffer_truncated: bool,
    pub thinking_animation_frame: usize,
    pub thinking_anchor: Option<usize>,
    pub(crate) thinking_result: ThinkingResult,
    pub usage: Usage,
    pub context_used_tokens: u64,
    pub context_limit_tokens: Option<u64>,
    pub context_meter_enabled: bool,
    pub pending_approval: Option<PendingApproval>,
    pub settings: Option<SettingsState>,
    pub palette: Option<CommandPaletteState>,
    pub mode: AgentMode,
    pub leader_pending: bool,
    pub expanded_tools: HashSet<String>,
    pub thinking_expanded: bool,
    pub thinking_menu_open: bool,
    pub thinking_control_rect: Option<Rect>,
    pub thinking_menu_rect: Option<Rect>,
    pub force_full_redraw: bool,
    pub mouse_press_target: Option<InteractionTarget>,
    pub mouse_press_position: Option<(u16, u16)>,
    pub mouse_dragged: bool,
    pub layout_restore_anchor: Option<(InteractionTarget, usize)>,
    pub file_suggestions: Vec<String>,
    pub file_selected: usize,
    pub message_scroll: usize,
    pub follow_output: bool,
    pub output_scroll_top: Option<usize>,
    pub output_selection: Option<OutputSelection>,
    pub message_layout: Option<MessageLayout>,
    pub output_layout_dirty: bool,
    #[cfg(test)]
    pub output_layout_rebuild_count: usize,
    #[cfg(test)]
    pub markdown_parse_count: usize,
    #[cfg(test)]
    pub footer_rebuild_count: usize,
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
        thinking_last_line: String::new(),
        thinking_active: false,
        thinking_buffer: String::new(),
        thinking_buffer_truncated: false,
        thinking_animation_frame: 0,
        thinking_anchor: None,
        thinking_result: ThinkingResult::Completed,
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
        thinking_expanded: false,
        thinking_menu_open: false,
        thinking_control_rect: None,
        thinking_menu_rect: None,
        force_full_redraw: false,
        mouse_press_target: None,
        mouse_press_position: None,
        mouse_dragged: false,
        layout_restore_anchor: None,
        file_suggestions: Vec::new(),
        file_selected: 0,
        message_scroll: 0,
        follow_output: true,
        output_scroll_top: None,
        output_selection: None,
        message_layout: None,
        output_layout_dirty: true,
        #[cfg(test)]
        output_layout_rebuild_count: 0,
        #[cfg(test)]
        markdown_parse_count: 0,
        #[cfg(test)]
        footer_rebuild_count: 0,
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
    // Best-effort kitty keyboard protocol enhancement. Terminals without
    // support ignore it and the legacy Windows console API returns
    // Unsupported, so a failure here must not prevent startup. This makes
    // Alt+Up/Down arrive as `KeyCode::Up`/`Down` with the ALT modifier set.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    if let Err(error) = terminal.clear() {
        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
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
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
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
    let mut thinking_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
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
        if app.thinking_active && thinking_timer.is_none() {
            thinking_timer = Some(Box::pin(tokio::time::sleep(
                std::time::Duration::from_millis(100),
            )));
        }
        if !app.thinking_active {
            thinking_timer = None;
        }
        let edge_scroll_tick = async {
            if let Some(timer) = edge_scroll_timer.as_mut() {
                timer.await;
            } else {
                pending::<()>().await;
            }
        };
        let thinking_tick = async {
            if let Some(timer) = thinking_timer.as_mut() {
                timer.await;
            } else {
                pending::<()>().await;
            }
        };
        let mut redraw = false;
        tokio::select! {
            _ = edge_scroll_tick => {
                edge_scroll_timer = None;
                auto_scroll_selection(app);
                redraw = true;
            }
            _ = thinking_tick => {
                thinking_timer = None;
                app.thinking_animation_frame = app.thinking_animation_frame.wrapping_add(1);
                redraw = true;
            }
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(event)) => {
                        let outcome = handle_terminal_event(app, event).await?;
                        redraw = outcome.redraw;
                        if let Some(sequence) = outcome.osc52 {
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
                    redraw = true;
                }
            }
        }
        if redraw {
            if app.force_full_redraw {
                terminal.clear()?;
                app.force_full_redraw = false;
            }
            terminal.draw(|frame| ui::draw(frame, app))?;
        }
    }
    if let Some(task) = app.active_task.take() {
        task.abort();
    }
    if let Some(approval) = take_pending_approval(app) {
        if let ApprovalAction::Agent(reply) = approval.action {
            let _ = reply.send(false);
        }
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct EventOutcome {
    redraw: bool,
    osc52: Option<String>,
}

impl EventOutcome {
    fn redraw() -> Self {
        Self {
            redraw: true,
            osc52: None,
        }
    }
}

async fn handle_terminal_event(app: &mut App, event: Event) -> Result<EventOutcome> {
    if let Event::Paste(text) = &event {
        if !app.busy && app.settings.is_none() && app.palette.is_none() {
            app.input.insert_str(text);
            update_file_suggestions(app);
            return Ok(EventOutcome::redraw());
        }
        return Ok(EventOutcome::default());
    }
    if let Event::Mouse(mouse) = event {
        if let Some(outcome) = handle_thinking_mouse(app, mouse)? {
            return Ok(outcome);
        }
        if output_mouse_event_allowed(
            mouse.kind,
            app.settings.is_some(),
            app.palette.is_some(),
            app.pending_approval.is_some(),
        ) {
            return Ok(handle_output_mouse(app, mouse));
        }
        return Ok(EventOutcome::default());
    }
    if matches!(event, Event::Resize(_, _)) {
        if app.pending_approval.is_some()
            || app.settings.is_some()
            || app.palette.is_some()
            || app.thinking_menu_open
        {
            app.force_full_redraw = true;
        }
        return Ok(EventOutcome::redraw());
    }
    let Event::Key(key) = event else {
        return Ok(EventOutcome::default());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(EventOutcome::default());
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return Ok(EventOutcome::default());
    }
    if app.settings.is_some() {
        let redraw = settings_key_handled(key.code, key.modifiers);
        handle_settings_key(app, key.code, key.modifiers);
        return Ok(EventOutcome {
            redraw,
            osc52: None,
        });
    }
    if app.palette.is_some() {
        let redraw = palette_key_handled(key.code, key.modifiers);
        handle_palette_key(app, key.code, key.modifiers);
        return Ok(EventOutcome {
            redraw,
            osc52: None,
        });
    }
    if app.pending_approval.is_some() {
        let redraw = matches!(
            key.code,
            KeyCode::Char('y')
                | KeyCode::Char('Y')
                | KeyCode::Char('n')
                | KeyCode::Char('N')
                | KeyCode::Esc
        );
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => resolve_approval(app, true),
            KeyCode::Char('n') | KeyCode::Char('N') => resolve_approval(app, false),
            KeyCode::Esc => cancel_active_request(app),
            _ => {}
        }
        return Ok(EventOutcome {
            redraw,
            osc52: None,
        });
    }
    let redraw = match key.code {
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            app.palette = Some(CommandPaletteState {
                query: String::new(),
                selected: 0,
            });
            app.status = "命令面板 | 输入筛选 | Enter 执行 | Esc 关闭".into();
            true
        }
        KeyCode::PageUp if !app.busy => {
            scroll_messages(app, 5);
            true
        }
        KeyCode::PageDown if !app.busy => {
            scroll_messages(app, -5);
            true
        }
        KeyCode::Up if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            scroll_messages(app, 3);
            true
        }
        KeyCode::Down if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            scroll_messages(app, -3);
            true
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            scroll_to_bottom(app);
            true
        }
        KeyCode::PageUp if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            scroll_messages(app, 5);
            true
        }
        KeyCode::PageDown if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            scroll_messages(app, -5);
            true
        }
        KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            app.leader_pending = true;
            app.status = "快捷键：n 新建 | s 设置 | f 分支 | p 面板 | q 退出".into();
            true
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
            true
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            open_settings(app);
            true
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            create_session(app)?;
            true
        }
        KeyCode::Up if !app.busy && session_switch_direction(&key) == Some(-1) => {
            switch_session(app, -1)?;
            true
        }
        KeyCode::Down if !app.busy && session_switch_direction(&key) == Some(1) => {
            switch_session(app, 1)?;
            true
        }
        KeyCode::Esc => {
            app.leader_pending = false;
            if let Some(task) = app.active_task.take() {
                task.abort();
                finish_thinking(app, "思考已取消");
                app.busy = false;
                app.agent_phase = AgentPhase::Idle;
                app.model_phase = ModelPhase::Idle;
                app.status = "已取消".into();
                app.push_entry(DisplayEntry {
                    kind: DisplayKind::System,
                    content: DisplayContent::Markdown("请求已取消。".into()),
                });
            }
            true
        }
        KeyCode::Tab if !app.busy && !app.file_suggestions.is_empty() => {
            apply_file_completion(app);
            true
        }
        KeyCode::Enter if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.insert('\n');
            app.file_suggestions.clear();
            true
        }
        KeyCode::Char('j') if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.insert('\n');
            app.file_suggestions.clear();
            true
        }
        KeyCode::Enter if !app.busy => {
            submit_input(app)?;
            true
        }
        KeyCode::Backspace if !app.busy => {
            app.input.backspace();
            update_file_suggestions(app);
            true
        }
        KeyCode::Delete if !app.busy => {
            app.input.delete();
            update_file_suggestions(app);
            true
        }
        KeyCode::Left if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.select_left();
            true
        }
        KeyCode::Right if !app.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.select_right();
            true
        }
        KeyCode::Char('a') if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.select_all();
            true
        }
        KeyCode::Left if !app.busy => {
            app.input.move_left();
            true
        }
        KeyCode::Right if !app.busy => {
            app.input.move_right();
            true
        }
        KeyCode::Home if !app.busy => {
            app.input.move_home();
            true
        }
        KeyCode::End if !app.busy => {
            app.input.move_end();
            true
        }
        KeyCode::Up
            if !app.busy && key.modifiers.is_empty() && !app.file_suggestions.is_empty() =>
        {
            app.file_selected = app.file_selected.saturating_sub(1);
            true
        }
        KeyCode::Down
            if !app.busy && key.modifiers.is_empty() && !app.file_suggestions.is_empty() =>
        {
            app.file_selected =
                (app.file_selected + 1).min(app.file_suggestions.len().saturating_sub(1));
            true
        }
        KeyCode::Up if !app.busy && key.modifiers.is_empty() => {
            if app.input.is_empty() {
                app.input.history_previous();
            } else {
                app.input.move_up();
            }
            true
        }
        KeyCode::Down if !app.busy && key.modifiers.is_empty() => {
            if app.input.is_empty() {
                app.input.history_next();
            } else {
                app.input.move_down();
            }
            true
        }
        KeyCode::Char('w') if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_word_left();
            true
        }
        KeyCode::Char('u') if !app.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.clear();
            true
        }
        KeyCode::Char(character)
            if !app.busy
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.input.insert(character);
            update_file_suggestions(app);
            true
        }
        _ => false,
    };
    Ok(EventOutcome {
        redraw,
        osc52: None,
    })
}

fn handle_thinking_mouse(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
) -> Result<Option<EventOutcome>> {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return Ok(app.thinking_menu_open.then(EventOutcome::default));
    }
    if app.thinking_menu_open {
        let selected = app
            .thinking_menu_rect
            .filter(|rect| point_in_rect(mouse.column, mouse.row, *rect))
            .and_then(|rect| thinking_menu_selection(app, rect, mouse.column, mouse.row));
        app.thinking_menu_open = false;
        app.force_full_redraw = true;
        if let Some((level, budget)) = selected {
            apply_thinking_selection(app, level, budget)?;
        }
        return Ok(Some(EventOutcome::redraw()));
    }
    if !app.busy
        && app.pending_approval.is_none()
        && app
            .thinking_control_rect
            .is_some_and(|rect| point_in_rect(mouse.column, mouse.row, rect))
    {
        app.thinking_menu_open = true;
        return Ok(Some(EventOutcome::redraw()));
    }
    Ok(None)
}

fn point_in_rect(column: u16, row: u16, rect: Rect) -> bool {
    column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
}

fn thinking_menu_selection(
    app: &App,
    rect: Rect,
    column: u16,
    row: u16,
) -> Option<(ThinkingLevel, Option<u32>)> {
    let inner = ratatui::widgets::Block::bordered().inner(rect);
    if !point_in_rect(column, row, inner) {
        return None;
    }
    let profile = app.thinking_profile();
    let index = row.saturating_sub(inner.y) as usize;
    if profile.kind == crate::config::ThinkingProfileKind::Qwen37
        && column >= inner.x.saturating_add(8)
    {
        const BUDGETS: [Option<u32>; 6] = [
            None,
            Some(1024),
            Some(4096),
            Some(8192),
            Some(16384),
            Some(32768),
        ];
        return BUDGETS
            .get(index)
            .copied()
            .map(|budget| (ThinkingLevel::Enabled, budget));
    }
    profile.options.get(index).copied().map(|level| {
        let budget = (level == ThinkingLevel::Enabled)
            .then_some(app.config.provider.thinking_budget_tokens)
            .flatten();
        (level, budget)
    })
}

fn apply_thinking_selection(
    app: &mut App,
    level: ThinkingLevel,
    budget: Option<u32>,
) -> Result<()> {
    app.config.provider.thinking_level = level;
    app.config.provider.thinking_budget_tokens = budget;
    app.config.provider.normalize_thinking();
    rebuild_runner(app)?;
    app.status = match app.config.save() {
        Ok(()) => format!(
            "思考强度已设为 {}",
            app.config.provider.thinking_level.label()
        ),
        Err(error) => format!(
            "思考强度已更新；配置保存失败：{}",
            secrets::redact(&error.to_string())
        ),
    };
    Ok(())
}

fn handle_output_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) -> EventOutcome {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            scroll_messages(app, MOUSE_WHEEL_SCROLL_LINES);
            EventOutcome::redraw()
        }
        MouseEventKind::ScrollDown => {
            scroll_messages(app, -MOUSE_WHEEL_SCROLL_LINES);
            EventOutcome::redraw()
        }
        MouseEventKind::Down(MouseButton::Left) => {
            app.mouse_dragged = false;
            app.mouse_press_position = Some((mouse.column, mouse.row));
            app.mouse_press_target = app
                .message_layout
                .as_ref()
                .and_then(|layout| layout.interaction_at(mouse.column, mouse.row));
            if app.mouse_press_target.is_some() {
                app.clear_output_selection();
                app.edge_scroll = EdgeScroll::default();
                return EventOutcome::redraw();
            }
            let Some(offset) = app
                .message_layout
                .as_ref()
                .and_then(|layout| layout.hit_test(mouse.column, mouse.row))
            else {
                app.clear_output_selection();
                return EventOutcome::redraw();
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
            EventOutcome::redraw()
        }
        MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
            if app.mouse_press_target.is_some() {
                let moved = matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left))
                    || app.mouse_press_position.is_some_and(|(column, row)| {
                        column.abs_diff(mouse.column) > 1 || row.abs_diff(mouse.row) > 1
                    });
                if moved {
                    app.mouse_dragged = true;
                    app.mouse_press_target = None;
                    if let Some((column, row)) = app.mouse_press_position
                        && let Some(offset) = app
                            .message_layout
                            .as_ref()
                            .and_then(|layout| layout.hit_test(column, row))
                    {
                        app.output_selection = Some(OutputSelection::new(offset));
                    }
                    update_drag_position(app, mouse.column, mouse.row);
                    return EventOutcome::redraw();
                }
                return EventOutcome::default();
            }
            if app
                .output_selection
                .is_some_and(|selection| selection.dragging)
            {
                update_drag_position(app, mouse.column, mouse.row);
                EventOutcome::redraw()
            } else {
                EventOutcome::default()
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.edge_scroll = EdgeScroll::default();
            let pressed_target = app.mouse_press_target.take();
            app.mouse_press_position = None;
            if let Some(target) = pressed_target {
                let released_target = app
                    .message_layout
                    .as_ref()
                    .and_then(|layout| layout.interaction_at(mouse.column, mouse.row));
                if !app.mouse_dragged && released_target.as_ref() == Some(&target) {
                    if !app.follow_output {
                        app.layout_restore_anchor =
                            app.message_layout.as_ref().and_then(|layout| {
                                layout
                                    .visual_lines
                                    .iter()
                                    .position(|line| line.interaction.as_ref() == Some(&target))
                                    .map(|visual_row| {
                                        (target.clone(), visual_row.saturating_sub(layout.scroll))
                                    })
                            });
                    }
                    match target {
                        InteractionTarget::Tool(call_id) => {
                            if !app.expanded_tools.insert(call_id.clone()) {
                                app.expanded_tools.remove(&call_id);
                            }
                        }
                        InteractionTarget::Thinking => {
                            app.thinking_expanded = !app.thinking_expanded;
                        }
                    }
                    app.invalidate_output_layout();
                }
                app.mouse_dragged = false;
                return EventOutcome::redraw();
            }
            app.mouse_dragged = false;
            let Some(mut selection) = app.output_selection else {
                return EventOutcome::default();
            };
            selection.dragging = false;
            let Some((start, end)) = selection.range() else {
                app.output_selection = None;
                return EventOutcome::redraw();
            };
            app.output_selection = Some(selection);
            let Some(text) = app
                .message_layout
                .as_ref()
                .and_then(|layout| layout.text.get(start..end))
                .map(str::to_owned)
            else {
                app.status = "复制失败：选区位置已失效".into();
                return EventOutcome::redraw();
            };
            match crate::clipboard::copy_text(&text) {
                crate::clipboard::CopyResult::Native => {
                    app.status = "系统剪贴板已复制".into();
                    EventOutcome::redraw()
                }
                crate::clipboard::CopyResult::Osc52Requested(sequence) => {
                    app.status = "已向终端发送复制请求".into();
                    EventOutcome {
                        redraw: true,
                        osc52: Some(sequence),
                    }
                }
                crate::clipboard::CopyResult::Error(error) => {
                    app.status = format!("复制失败：{error}");
                    EventOutcome::redraw()
                }
            }
        }
        _ => EventOutcome::default(),
    }
}

fn palette_key_handled(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(
        code,
        KeyCode::Esc | KeyCode::Enter | KeyCode::Up | KeyCode::Down | KeyCode::Backspace
    ) || matches!(code, KeyCode::Char(_) if !modifiers.contains(KeyModifiers::CONTROL))
}

fn settings_key_handled(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(
        code,
        KeyCode::Esc
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Backspace
            | KeyCode::Enter
    ) || matches!(code, KeyCode::Char(_) if !modifiers.contains(KeyModifiers::CONTROL))
}

fn update_drag_position(app: &mut App, column: u16, row: u16) {
    update_edge_scroll(app, column, row);
    let Some(offset) = app.message_layout.as_ref().and_then(|layout| {
        let clamped_row = row
            .max(layout.viewport.y)
            .min(layout.viewport.bottom().saturating_sub(1));
        layout.hit_test(column, clamped_row)
    }) else {
        return;
    };
    if let Some(selection) = &mut app.output_selection {
        selection.active = offset;
    }
}

fn update_edge_scroll(app: &mut App, column: u16, row: u16) {
    let Some(layout) = &app.message_layout else {
        return;
    };
    let direction = edge_scroll_direction(row, layout.viewport);
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

const EDGE_SCROLL_ROWS: u16 = 1;

fn edge_scroll_direction(row: u16, viewport: ratatui::layout::Rect) -> i8 {
    if viewport.height == 0 {
        return 0;
    }
    let top_edge = viewport
        .y
        .saturating_add(EDGE_SCROLL_ROWS.saturating_sub(1));
    let bottom_edge = viewport.bottom().saturating_sub(EDGE_SCROLL_ROWS);
    if row <= top_edge {
        -1
    } else if row >= bottom_edge {
        1
    } else {
        0
    }
}

fn clear_output_selection(app: &mut App) {
    app.output_selection = None;
    app.edge_scroll = EdgeScroll::default();
}

fn push_entry(app: &mut App, entry: DisplayEntry) {
    clear_output_selection(app);
    app.invalidate_output_layout();
    app.entries.push(entry);
}

impl App {
    pub(crate) fn provider_label(&self) -> &'static str {
        self.config.provider.preset.label()
    }

    pub(crate) fn model_name(&self) -> &str {
        &self.config.provider.model
    }

    pub(crate) fn thinking_level(&self) -> ThinkingLevel {
        self.config.provider.thinking_level
    }

    pub(crate) fn thinking_budget_tokens(&self) -> Option<u32> {
        self.config.provider.thinking_budget_tokens
    }

    pub(crate) fn thinking_profile(&self) -> ThinkingProfile {
        thinking_profile(self.config.provider.preset, &self.config.provider.model)
    }

    fn clear_output_selection(&mut self) {
        clear_output_selection(self);
    }

    fn push_entry(&mut self, entry: DisplayEntry) {
        push_entry(self, entry);
    }

    pub fn invalidate_output_layout(&mut self) {
        self.message_layout.take();
        self.output_layout_dirty = true;
    }
}

fn cancel_active_request(app: &mut App) {
    if let Some(approval) = take_pending_approval(app)
        && let ApprovalAction::Agent(reply) = approval.action
    {
        let _ = reply.send(false);
    }
    if let Some(task) = app.active_task.take() {
        task.abort();
    }
    finish_thinking(app, "思考已取消");
    app.busy = false;
    app.agent_phase = AgentPhase::Idle;
    app.model_phase = ModelPhase::Idle;
    app.status = "已取消当前请求".into();
    app.push_entry(DisplayEntry {
        kind: DisplayKind::System,
        content: DisplayContent::Markdown("当前请求已取消。".into()),
    });
}

fn take_pending_approval(app: &mut App) -> Option<PendingApproval> {
    let approval = app.pending_approval.take();
    if approval.is_some() {
        app.force_full_redraw = true;
    }
    approval
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
    let next = next_output_scroll_top(current, max_scroll, delta);
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

fn next_output_scroll_top(current: usize, max_scroll: usize, delta: isize) -> usize {
    let current = current.min(max_scroll);
    if delta > 0 {
        current.saturating_sub(delta as usize)
    } else {
        current.saturating_add(delta.unsigned_abs()).min(max_scroll)
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
                    app.config.provider.normalize_thinking();
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
            app.invalidate_output_layout();
            app.entries.clear();
            reset_thinking_state(app);
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
        SettingsField::Thinking => {
            let current = ThinkingCapability::ALL
                .iter()
                .position(|value| *value == settings.provider.thinking)
                .unwrap_or(0) as i32;
            let next =
                (current + direction).rem_euclid(ThinkingCapability::ALL.len() as i32) as usize;
            settings.provider.thinking = ThinkingCapability::ALL[next];
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
    provider_config.normalize_thinking();
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
        AgentEvent::ReasoningDelta(delta) => {
            app.agent_phase = AgentPhase::Thinking;
            app.model_phase = ModelPhase::Streaming;
            update_thinking_line(app, &delta);
        }
        AgentEvent::ModelStreaming => {
            begin_thinking(app);
            app.agent_phase = AgentPhase::Thinking;
            app.model_phase = ModelPhase::Streaming;
            app.status = "等待模型流式响应".into();
        }
        AgentEvent::WebSearchStarted { query } => {
            finish_thinking(app, "思考完成");
            app.agent_phase = AgentPhase::ToolRunning;
            app.model_phase = ModelPhase::Streaming;
            let already_open = app.entries.last().is_some_and(|entry| {
                matches!(&entry.content, DisplayContent::Tool(tool) if tool.name == "web_search" && tool.status == ToolDisplayStatus::Running)
            });
            if !already_open {
                let call_id = format!("native-web-search-{}", uuid::Uuid::new_v4());
                app.push_entry(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::Tool(ToolDisplay {
                        call_id,
                        name: "web_search".into(),
                        arguments: serde_json::json!({"query": query}),
                        status: ToolDisplayStatus::Running,
                        result: None,
                    }),
                });
            }
            app.status = "正在联网搜索".into();
        }
        AgentEvent::WebSearchResult {
            title,
            url,
            snippet,
        } => {
            let context = format!("{title}\n{url}\n{snippet}");
            if let Some(tool) =
                app.entries
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match &mut entry.content {
                        DisplayContent::Tool(tool)
                            if tool.name == "web_search"
                                && tool.status == ToolDisplayStatus::Running =>
                        {
                            Some(tool)
                        }
                        _ => None,
                    })
            {
                let result = tool.result.get_or_insert_with(String::new);
                if !result.is_empty() {
                    result.push_str("\n\n");
                }
                result.push_str(&context);
            }
        }
        AgentEvent::WebSearchCompleted { count } => {
            if let Some(tool) =
                app.entries
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match &mut entry.content {
                        DisplayContent::Tool(tool)
                            if tool.name == "web_search"
                                && tool.status == ToolDisplayStatus::Running =>
                        {
                            Some(tool)
                        }
                        _ => None,
                    })
            {
                tool.status = ToolDisplayStatus::Completed;
                app.invalidate_output_layout();
            }
            app.agent_phase = AgentPhase::Thinking;
            app.status = if count == 0 {
                "联网搜索完成".into()
            } else {
                format!("联网搜索完成：{count} 条结果")
            };
        }
        AgentEvent::Cancelled(reason) => {
            finish_thinking(app, "思考已取消");
            app.busy = false;
            app.active_task = None;
            if let Some(approval) = take_pending_approval(app)
                && let ApprovalAction::Agent(reply) = approval.action
            {
                let _ = reply.send(false);
            }
            app.agent_phase = AgentPhase::Idle;
            app.model_phase = ModelPhase::Idle;
            app.status = if reason.contains("approval") {
                "审批等待已取消".into()
            } else {
                "请求已取消".into()
            };
        }
        AgentEvent::TextDelta(delta) => {
            finish_thinking(app, "思考完成");
            app.agent_phase = AgentPhase::StreamingText;
            app.model_phase = ModelPhase::Streaming;
            app.invalidate_output_layout();
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
            finish_thinking(app, "思考完成");
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
            finish_thinking(app, "思考完成");
            app.agent_phase = AgentPhase::ToolRunning;
            app.model_phase = ModelPhase::Idle;
            app.status = format!("正在执行 {}……", ui::tool_display_name(&call.name));
            app.push_entry(DisplayEntry {
                kind: DisplayKind::Tool,
                content: DisplayContent::Tool(ToolDisplay {
                    call_id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                    status: ToolDisplayStatus::Running,
                    result: None,
                }),
            });
        }
        AgentEvent::ToolFinished { call, result } => {
            app.agent_phase = AgentPhase::Thinking;
            let status = tool_result_status(&result);
            if let Some(tool) =
                app.entries
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match &mut entry.content {
                        DisplayContent::Tool(tool) if tool.call_id == call.id => Some(tool),
                        _ => None,
                    })
            {
                tool.status = status;
                tool.result = Some(result);
                app.invalidate_output_layout();
            } else {
                app.push_entry(DisplayEntry {
                    kind: DisplayKind::Tool,
                    content: DisplayContent::Tool(ToolDisplay {
                        call_id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                        status,
                        result: Some(result),
                    }),
                });
            }
            app.status = "正在将工具结果交给模型……".into();
        }
        AgentEvent::Usage(usage) => {
            app.context_used_tokens = usage
                .input_tokens
                .max(estimate_context_tokens(&app.conversation));
            app.usage = usage;
        }
        AgentEvent::Completed { items } => {
            finish_thinking(app, "思考完成");
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
            finish_thinking(app, "思考失败");
            app.push_entry(DisplayEntry {
                kind: DisplayKind::Error,
                content: DisplayContent::Markdown(secrets::redact(&error)),
            });
            app.busy = false;
            app.active_task = None;
            app.agent_phase = AgentPhase::Failed;
            app.model_phase = ModelPhase::Failed;
            app.status = "请求失败".into();
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
                content: DisplayContent::Tool(ToolDisplay {
                    call_id: format!("local-shell-{}", uuid::Uuid::new_v4()),
                    name: "terminal_shell".into(),
                    arguments: serde_json::json!({"command": command}),
                    status: tool_result_status(&result),
                    result: Some(result.clone()),
                }),
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

fn tool_result_status(result: &str) -> ToolDisplayStatus {
    let lower = result.to_ascii_lowercase();
    if lower.starts_with("rejected by user") || lower.starts_with("denied by policy") {
        ToolDisplayStatus::Rejected
    } else if lower.starts_with("tool failed")
        || lower.starts_with("security policy denied")
        || lower.starts_with("process timed out")
        || lower.starts_with("duplicate tool call")
    {
        ToolDisplayStatus::Failed
    } else {
        ToolDisplayStatus::Completed
    }
}

const MAX_THINKING_LINE_BYTES: usize = 1024;
const MAX_THINKING_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThinkingResult {
    #[default]
    Completed,
    Failed,
    Cancelled,
}

// Kept as a small pure helper so terminals without Braille support can use the
// ASCII sequence without changing the live row or layout.
pub(crate) fn thinking_animation_glyph(frame: usize, braille: bool) -> char {
    if braille {
        ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][frame % 10]
    } else {
        ['|', '/', '-', '\\'][frame % 4]
    }
}

pub(crate) fn braille_spinner_supported() -> bool {
    if std::env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb")) {
        return false;
    }
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .is_none_or(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        })
}

fn begin_thinking(app: &mut App) {
    // Anchor the single live row before the next entry. TextDelta will append
    // that assistant entry at the same index, so the row never moves.
    app.invalidate_output_layout();
    app.thinking_active = true;
    app.thinking_last_line = "模型正在思考".into();
    app.thinking_buffer.clear();
    app.thinking_buffer_truncated = false;
    app.thinking_animation_frame = 0;
    app.thinking_anchor = Some(app.entries.len());
    app.thinking_expanded = false;
}

fn finish_thinking(app: &mut App, line: &str) {
    app.thinking_active = false;
    app.thinking_animation_frame = 0;
    match line {
        "思考失败" => app.thinking_result = ThinkingResult::Failed,
        "思考已取消" => app.thinking_result = ThinkingResult::Cancelled,
        _ => app.thinking_result = ThinkingResult::Completed,
    }
}

fn reset_thinking_state(app: &mut App) {
    app.thinking_active = false;
    app.thinking_last_line.clear();
    app.thinking_buffer.clear();
    app.thinking_buffer_truncated = false;
    app.thinking_animation_frame = 0;
    app.thinking_anchor = None;
    app.thinking_result = ThinkingResult::Completed;
}

fn update_thinking_line(app: &mut App, delta: &str) {
    app.thinking_active = true;
    app.thinking_buffer.push_str(delta);
    if app.thinking_buffer.len() > MAX_THINKING_BUFFER_BYTES {
        let minimum = app
            .thinking_buffer
            .len()
            .saturating_sub(MAX_THINKING_BUFFER_BYTES);
        let start = app
            .thinking_buffer
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .find(|offset| *offset >= minimum)
            .unwrap_or(app.thinking_buffer.len());
        app.thinking_buffer.drain(..start);
        app.thinking_buffer_truncated = true;
    }
    let latest = app
        .thinking_buffer
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("思考中");
    app.thinking_last_line = utf8_tail(latest, MAX_THINKING_LINE_BYTES).to_owned();
}

fn utf8_tail(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let minimum = value.len().saturating_sub(max_bytes);
    let start = value
        .grapheme_indices(true)
        .map(|(offset, _)| offset)
        .find(|offset| *offset >= minimum)
        .unwrap_or(value.len());
    &value[start..]
}

fn resolve_approval(app: &mut App, approved: bool) {
    if let Some(approval) = take_pending_approval(app) {
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

/// Returns the session switch direction for keys dedicated to moving through
/// the session list: `Alt+Up`/`Alt+Down` and, for backwards compatibility,
/// `Ctrl+Up`/`Ctrl+Down`. Bare Up/Down must stay with the input editor, so they
/// deliberately return `None`.
fn session_switch_direction(key: &crossterm::event::KeyEvent) -> Option<i32> {
    let has_switch_modifier = key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
    if !has_switch_modifier {
        return None;
    }
    match key.code {
        KeyCode::Up => Some(-1),
        KeyCode::Down => Some(1),
        _ => None,
    }
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
    app.invalidate_output_layout();
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
    reset_thinking_state(app);
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
    let mut tool_entries = HashMap::<String, usize>::new();
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
                        content: DisplayContent::Tool(ToolDisplay {
                            call_id: item
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("native-web-search-{}", entries.len())),
                            name: "web_search".into(),
                            arguments: item.get("action").cloned().unwrap_or_default(),
                            status: ToolDisplayStatus::Completed,
                            result: None,
                        }),
                    });
                }
            }
            ConversationItem::AssistantToolCalls { calls } => {
                for call in calls {
                    tool_entries.insert(call.id.clone(), entries.len());
                    entries.push(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::Tool(ToolDisplay {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            status: ToolDisplayStatus::Running,
                            result: None,
                        }),
                    });
                }
            }
            ConversationItem::ToolOutput { call_id, output } => {
                if let Some(tool) = tool_entries
                    .get(call_id)
                    .and_then(|index| entries.get_mut(*index))
                    .and_then(|entry| match &mut entry.content {
                        DisplayContent::Tool(tool) => Some(tool),
                        _ => None,
                    })
                {
                    tool.status = tool_result_status(output);
                    tool.result = Some(output.clone());
                } else {
                    entries.push(DisplayEntry {
                        kind: DisplayKind::Tool,
                        content: DisplayContent::Tool(ToolDisplay {
                            call_id: call_id.clone(),
                            name: "tool".into(),
                            arguments: Value::Null,
                            status: tool_result_status(output),
                            result: Some(output.clone()),
                        }),
                    });
                }
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

fn trim_entries(entries: &mut Vec<DisplayEntry>) -> usize {
    const MAX_ENTRIES: usize = 1000;
    const MAX_BYTES: usize = 2 * 1024 * 1024;
    let mut removed = 0;
    while entries.len() > MAX_ENTRIES || display_entry_bytes(entries) > MAX_BYTES {
        if entries.len() > MAX_ENTRIES {
            let count = entries.len() - MAX_ENTRIES;
            entries.drain(..count);
            removed += count;
        } else {
            entries.remove(0);
            removed += 1;
        }
    }
    removed
}

fn trim_app_entries(app: &mut App) {
    const MAX_ENTRIES: usize = 1000;
    const MAX_BYTES: usize = 2 * 1024 * 1024;
    if app.entries.len() <= MAX_ENTRIES && display_entry_bytes(&app.entries) <= MAX_BYTES {
        return;
    }
    app.invalidate_output_layout();
    let removed = trim_entries(&mut app.entries);
    app.thinking_anchor = app
        .thinking_anchor
        .map(|anchor| anchor.saturating_sub(removed));
    app.clear_output_selection();
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
            DisplayContent::Tool(tool) => {
                tool.call_id.len()
                    + tool.name.len()
                    + tool.arguments.to_string().len()
                    + tool.result.as_ref().map_or(0, String::len)
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers, MouseEvent};
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn session_switch_direction_only_accepts_alt_or_ctrl_arrows() {
        assert_eq!(
            session_switch_direction(&KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
            Some(-1)
        );
        assert_eq!(
            session_switch_direction(&KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
            Some(1)
        );
        // Keep the pre-existing Ctrl+Up/Down behaviour.
        assert_eq!(
            session_switch_direction(&KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(-1)
        );
        assert_eq!(
            session_switch_direction(&KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)),
            Some(1)
        );
        assert_eq!(
            session_switch_direction(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            session_switch_direction(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            session_switch_direction(&KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)),
            None
        );
        assert_eq!(
            session_switch_direction(&KeyEvent::new(KeyCode::PageUp, KeyModifiers::ALT)),
            None
        );
    }

    fn test_app(temp: &TempDir) -> App {
        let workspace = temp.path().to_path_buf();
        let config = Config::default();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
        let session_id = storage.create_session(&workspace).unwrap();
        let sessions = storage.list_sessions(&workspace).unwrap();
        let registry = Arc::new(ToolRegistry::new(
            Workspace::new(&workspace).unwrap(),
            config.runtime.clone(),
            config.security.allow_private_networks,
        ));
        let (agent_tx, agent_rx) = mpsc::channel(8);
        App {
            workspace,
            input: InputBuffer::new(),
            entries: vec![DisplayEntry {
                kind: DisplayKind::Assistant,
                content: DisplayContent::Markdown("first line\n\n中文 🙂 long output".into()),
            }],
            status: String::new(),
            busy: false,
            agent_phase: AgentPhase::Idle,
            model_phase: ModelPhase::Idle,
            thinking_last_line: String::new(),
            thinking_active: false,
            thinking_buffer: String::new(),
            thinking_buffer_truncated: false,
            thinking_animation_frame: 0,
            thinking_anchor: None,
            thinking_result: ThinkingResult::Completed,
            usage: Usage::default(),
            context_used_tokens: 1,
            context_limit_tokens: None,
            context_meter_enabled: false,
            pending_approval: None,
            settings: None,
            palette: None,
            mode: AgentMode::default(),
            leader_pending: false,
            expanded_tools: HashSet::new(),
            thinking_expanded: false,
            thinking_menu_open: false,
            thinking_control_rect: None,
            thinking_menu_rect: None,
            force_full_redraw: false,
            mouse_press_target: None,
            mouse_press_position: None,
            mouse_dragged: false,
            layout_restore_anchor: None,
            file_suggestions: Vec::new(),
            file_selected: 0,
            message_scroll: 0,
            follow_output: true,
            output_scroll_top: None,
            output_selection: None,
            message_layout: None,
            output_layout_dirty: true,
            output_layout_rebuild_count: 0,
            markdown_parse_count: 0,
            footer_rebuild_count: 0,
            edge_scroll: EdgeScroll::default(),
            session_id,
            sessions,
            conversation: Vec::new(),
            storage,
            config,
            registry,
            runner: None,
            active_secret: None,
            agent_tx,
            agent_rx,
            active_task: None,
            should_quit: false,
        }
    }

    fn render_screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn activity_priority_and_contextual_shortcuts_are_stable() {
        use crate::{
            ui_layout::{Density, HeightClass},
            ui_view_model::{ActivityState, UiViewModel, activity_view, contextual_shortcuts},
        };

        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        assert_eq!(activity_view(&app).state, ActivityState::Idle);

        app.agent_phase = AgentPhase::StreamingText;
        assert_eq!(activity_view(&app).text, "正在生成回复");
        app.agent_phase = AgentPhase::Thinking;
        assert_eq!(activity_view(&app).text, "正在思考");
        app.agent_phase = AgentPhase::ToolRunning;
        assert!(activity_view(&app).text.starts_with("正在执行："));
        app.agent_phase = AgentPhase::Failed;
        assert_eq!(activity_view(&app).state, ActivityState::Failed);

        let (reply, _receiver) = oneshot::channel();
        app.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "approval".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/ui.rs"}),
            },
            reason: "test".into(),
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });
        assert_eq!(activity_view(&app).state, ActivityState::Warning);
        assert_eq!(activity_view(&app).text, "文件修改需要确认");
        assert_eq!(contextual_shortcuts(&app)[0].key, "Y");

        let view = UiViewModel::from_app(&app, Density::Compact, HeightClass::Normal, 44);
        assert_eq!(view.footer.primary.left[1].text, "文件修改需要确认");
        assert!(
            view.footer
                .secondary
                .as_ref()
                .is_some_and(|line| line.left[0].text.contains("src/ui.rs"))
        );
    }

    #[test]
    fn footer_and_responsive_screens_render_without_overflow() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.context_meter_enabled = true;
        app.context_used_tokens = 87_000;
        app.context_limit_tokens = Some(258_000);

        let wide = render_screen(&mut app, 120, 30);
        assert!(wide[28].contains('○'));
        assert!(wide[28].contains("Enter"));
        assert!(wide[28].contains("Ctrl+P"));
        assert!(wide[29].contains("OpenAI"));
        assert!(wide[29].contains("33%"));
        assert!(wide[29].contains("87k/258k"));
        assert!(wide.iter().any(|line| line.contains("Alt+Up/Down")));

        app.busy = true;
        app.agent_phase = AgentPhase::Thinking;
        let narrow = render_screen(&mut app, 60, 20);
        assert!(narrow[18].contains('●'));
        assert!(narrow[18].contains("Esc"));
        assert!(narrow[19].contains("33%"));
        assert!(!narrow.iter().any(|line| line.contains("Alt+Up/Down")));

        let short = render_screen(&mut app, 44, 14);
        assert!(short[13].contains('●'));
        assert!(!short[13].contains("上下文"));

        let tiny = render_screen(&mut app, 2, 2);
        assert_eq!(tiny.len(), 2);
    }

    #[tokio::test]
    async fn thinking_menu_is_mouse_only_bounded_and_does_not_rebuild_output() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.config.provider = ProviderPreset::DeepSeek.defaults();
        app.config.provider.model = "tenant-deepseek-v4-flash-long-deployment-name".into();
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 60, 12));
        let rebuilds = app.output_layout_rebuild_count;
        let parses = app.markdown_parse_count;
        let screen = render_screen(&mut app, 60, 20);
        assert!(screen[19].contains("high ▾"));
        let control = app.thinking_control_rect.unwrap();
        assert_eq!(
            control.width as usize,
            unicode_width::UnicodeWidthStr::width("思考 high ▾")
        );
        let click = |column, row| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(
            handle_terminal_event(&mut app, click(control.x, control.y))
                .await
                .unwrap()
                .redraw
        );
        assert!(app.thinking_menu_open);
        render_screen(&mut app, 60, 20);
        let menu = app.thinking_menu_rect.unwrap();
        assert!(menu.right() <= 60 && menu.bottom() <= 20);
        let inner = ratatui::widgets::Block::bordered().inner(menu);
        let max_row = app
            .thinking_profile()
            .options
            .iter()
            .position(|level| *level == ThinkingLevel::Max)
            .unwrap() as u16;
        handle_terminal_event(&mut app, click(inner.x, inner.y + max_row))
            .await
            .unwrap();
        assert_eq!(app.thinking_level(), ThinkingLevel::Max);
        assert!(!app.thinking_menu_open);
        assert!(app.status.contains("配置保存失败"));
        assert_eq!(app.output_layout_rebuild_count, rebuilds);
        assert_eq!(app.markdown_parse_count, parses);

        app.busy = true;
        render_screen(&mut app, 60, 20);
        let control = app.thinking_control_rect.unwrap();
        handle_terminal_event(&mut app, click(control.x, control.y))
            .await
            .unwrap();
        assert!(!app.thinking_menu_open);
    }

    #[tokio::test]
    async fn clicking_outside_thinking_menu_closes_it_without_changing_level() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        render_screen(&mut app, 80, 20);
        let control = app.thinking_control_rect.unwrap();
        let event = |column, row| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        handle_terminal_event(&mut app, event(control.x, control.y))
            .await
            .unwrap();
        render_screen(&mut app, 80, 20);
        let previous = app.thinking_level();
        handle_terminal_event(&mut app, event(0, 0)).await.unwrap();
        assert!(!app.thinking_menu_open);
        assert_eq!(app.thinking_level(), previous);
        assert!(app.force_full_redraw);
    }

    #[test]
    fn approval_closure_paths_request_one_full_redraw() {
        for approved in [true, false] {
            let temp = TempDir::new().unwrap();
            let mut app = test_app(&temp);
            let (reply, _receiver) = oneshot::channel();
            app.pending_approval = Some(PendingApproval {
                call: ToolCall {
                    id: "approval".into(),
                    name: "file_write".into(),
                    arguments: serde_json::json!({"path":"src/ui.rs"}),
                },
                reason: "risk text".into(),
                action: ApprovalAction::Agent(reply),
                created_at: Instant::now(),
            });
            resolve_approval(&mut app, approved);
            assert!(app.pending_approval.is_none());
            assert!(app.force_full_redraw);
        }

        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        request_shell_approval(&mut app, "echo ok".into()).unwrap();
        resolve_approval(&mut app, false);
        assert!(app.force_full_redraw);

        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let (reply, _receiver) = oneshot::channel();
        app.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "cancel".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({}),
            },
            reason: "cancel".into(),
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });
        cancel_active_request(&mut app);
        assert!(app.force_full_redraw);

        let (reply, _receiver) = oneshot::channel();
        app.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "event-cancel".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({}),
            },
            reason: "cancel".into(),
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });
        app.force_full_redraw = false;
        handle_agent_event(&mut app, AgentEvent::Cancelled("cancelled".into()));
        assert!(app.force_full_redraw);
    }

    #[test]
    fn approval_overlay_clear_restores_underlying_frame() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let (reply, _receiver) = oneshot::channel();
        app.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "approval".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/ui.rs"}),
            },
            reason: "unique-risk-text".into(),
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        resolve_approval(&mut app, false);
        assert!(app.force_full_redraw);
        terminal.clear().unwrap();
        app.force_full_redraw = false;
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!visible.contains("unique-risk-text"));
        assert!(!visible.contains("工具权限确认"));
        assert!(visible.contains("first line"));
    }

    #[test]
    fn ordinary_updates_do_not_request_full_redraw() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        scroll_messages(&mut app, 1);
        assert!(!app.force_full_redraw);
        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        handle_agent_event(&mut app, AgentEvent::ReasoningDelta("真实思考".into()));
        handle_agent_event(&mut app, AgentEvent::TextDelta("正文".into()));
        assert!(!app.force_full_redraw);
    }

    #[test]
    fn footer_updates_do_not_rebuild_messages_or_parse_markdown() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        render_screen(&mut app, 80, 20);
        let layout_rebuilds = app.output_layout_rebuild_count;
        let markdown_parses = app.markdown_parse_count;
        let footer_rebuilds = app.footer_rebuild_count;

        app.status = "仅 Footer 变化".into();
        render_screen(&mut app, 80, 20);
        assert_eq!(app.output_layout_rebuild_count, layout_rebuilds);
        assert_eq!(app.markdown_parse_count, markdown_parses);
        assert_eq!(app.footer_rebuild_count, footer_rebuilds + 1);
    }

    #[test]
    fn footer_keeps_context_visible_when_model_name_is_long() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.context_meter_enabled = true;
        app.context_used_tokens = 87_000;
        app.context_limit_tokens = Some(258_000);
        app.config.provider.model = "a-very-long-model-name-that-must-not-cover-context".into();
        let screen = render_screen(&mut app, 70, 20);
        assert!(screen[19].contains("33%"));
        assert!(screen[19].contains("87k/258k"));
    }

    #[test]
    fn approval_tool_failure_and_context_threshold_screens_are_distinct() {
        use crate::{
            ui_layout::{Density, HeightClass},
            ui_theme::VisualRole,
            ui_view_model::UiViewModel,
        };

        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let (reply, _receiver) = oneshot::channel();
        app.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "approval-screen".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/ui.rs"}),
            },
            reason: "将修改工作区文件".into(),
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });
        let approval = render_screen(&mut app, 100, 24);
        assert!(approval[22].contains('!'));
        assert!(approval[22].contains('Y'));
        assert!(approval[22].contains('N'));
        assert!(approval[23].contains("src/ui.rs"));
        app.pending_approval = None;

        app.agent_phase = AgentPhase::ToolRunning;
        app.entries.push(DisplayEntry {
            kind: DisplayKind::Tool,
            content: DisplayContent::Tool(ToolDisplay {
                call_id: "running-screen".into(),
                name: "file_read".into(),
                arguments: serde_json::json!({"path":"src/app.rs"}),
                status: ToolDisplayStatus::Running,
                result: None,
            }),
        });
        app.invalidate_output_layout();
        let tool = render_screen(&mut app, 100, 24);
        assert!(tool[22].contains('●'));
        assert!(tool.iter().any(|line| line.contains("src/app.rs")));

        app.agent_phase = AgentPhase::Failed;
        let failed = render_screen(&mut app, 100, 24);
        assert!(failed[22].contains('×'));

        app.context_meter_enabled = true;
        app.context_limit_tokens = Some(100);
        for (used, role) in [
            (70, VisualRole::Secondary),
            (85, VisualRole::Warning),
            (95, VisualRole::Danger),
        ] {
            app.context_used_tokens = used;
            let view = UiViewModel::from_app(&app, Density::Wide, HeightClass::Normal, 100);
            assert_eq!(view.footer.secondary.as_ref().unwrap().right[0].role, role);
            let screen = render_screen(&mut app, 100, 24);
            assert!(screen[23].contains(&format!("{used}%")));
        }
    }

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
        assert!(matches!(entries[2].kind, DisplayKind::Assistant));
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[1].content, DisplayContent::Tool(tool)
            if tool.call_id == "call_1" && tool.result.as_deref() == Some("ok")));
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
    fn top_based_scroll_translates_existing_scroll_semantics() {
        assert_eq!(next_output_scroll_top(5, 10, 2), 3);
        assert_eq!(next_output_scroll_top(5, 10, -2), 7);
        assert_eq!(next_output_scroll_top(0, 10, 3), 0);
        assert_eq!(next_output_scroll_top(10, 10, -3), 10);
        assert_eq!(next_output_scroll_top(99, 10, 0), 10);
    }

    #[test]
    fn edge_scroll_starts_on_the_first_visible_edge_row() {
        let viewport = Rect::new(30, 10, 40, 5);
        assert_eq!(edge_scroll_direction(9, viewport), -1);
        assert_eq!(edge_scroll_direction(10, viewport), -1);
        assert_eq!(edge_scroll_direction(11, viewport), 0);
        assert_eq!(edge_scroll_direction(13, viewport), 0);
        assert_eq!(edge_scroll_direction(14, viewport), 1);
        assert_eq!(edge_scroll_direction(15, viewport), 1);
    }

    #[test]
    fn edge_scroll_handles_zero_and_one_row_viewports() {
        assert_eq!(edge_scroll_direction(0, Rect::new(0, 0, 20, 0)), 0);
        assert_eq!(edge_scroll_direction(0, Rect::new(0, 0, 20, 1)), -1);
        assert_eq!(edge_scroll_direction(1, Rect::new(0, 0, 20, 1)), 1);
    }

    #[test]
    fn edge_scroll_direction_maps_to_top_based_motion() {
        let viewport = Rect::new(30, 10, 40, 5);
        let top_direction = edge_scroll_direction(10, viewport);
        let bottom_direction = edge_scroll_direction(14, viewport);
        assert_eq!(top_direction, -1);
        assert_eq!(bottom_direction, 1);
        assert_eq!(next_output_scroll_top(5, 10, 1), 4);
        assert_eq!(next_output_scroll_top(5, 10, -1), 6);
        assert_eq!(next_output_scroll_top(0, 10, 1), 0);
        assert_eq!(next_output_scroll_top(10, 10, -1), 10);
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

    #[test]
    fn scrolling_and_height_changes_reuse_the_complete_layout() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 8, 3);
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.output_layout_rebuild_count, 1);
        let markdown_parses = app.markdown_parse_count;

        let layout = app.message_layout.as_ref().unwrap();
        let text_ptr = layout.text.as_ptr();
        let lines_ptr = layout.lines.as_ptr();
        let visual_lines_ptr = layout.visual_lines.as_ptr();
        let line_count = layout.lines.len();
        let visual_line_count = layout.visual_lines.len();
        scroll_messages(&mut app, 1);
        crate::ui::update_message_layout(&mut app, viewport);
        scroll_messages(&mut app, -1);
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 8, 5));

        let layout = app.message_layout.as_ref().unwrap();
        assert_eq!(app.output_layout_rebuild_count, 1);
        assert_eq!(app.markdown_parse_count, markdown_parses);
        assert_eq!(layout.text.as_ptr(), text_ptr);
        assert_eq!(layout.lines.as_ptr(), lines_ptr);
        assert_eq!(layout.visual_lines.as_ptr(), visual_lines_ptr);
        assert_eq!(layout.lines.len(), line_count);
        assert_eq!(layout.visual_lines.len(), visual_line_count);
    }

    #[test]
    fn width_change_reflows_exactly_once_without_reparsing_text() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 8, 3));
        let text_ptr = app.message_layout.as_ref().unwrap().text.as_ptr();
        let lines_ptr = app.message_layout.as_ref().unwrap().lines.as_ptr();
        let markdown_parses = app.markdown_parse_count;

        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 16, 3));
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 16, 3));
        let layout = app.message_layout.as_ref().unwrap();
        assert_eq!(app.output_layout_rebuild_count, 2);
        assert_eq!(app.markdown_parse_count, markdown_parses);
        assert_eq!(layout.text.as_ptr(), text_ptr);
        assert_eq!(layout.lines.as_ptr(), lines_ptr);
    }

    #[test]
    fn text_delta_invalidates_and_rebuilds_with_new_text() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 4);
        crate::ui::update_message_layout(&mut app, viewport);
        handle_agent_event(&mut app, AgentEvent::TextDelta(" NEW".into()));
        assert!(app.output_layout_dirty);
        assert!(app.message_layout.is_none());

        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.output_layout_rebuild_count, 2);
        assert!(app.message_layout.as_ref().unwrap().text.contains("NEW"));
    }

    #[test]
    fn reasoning_deltas_keep_only_latest_line_out_of_history_and_storage() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 4);
        crate::ui::update_message_layout(&mut app, viewport);
        let entries = app.entries.len();
        let stored = app.storage.load_messages(&app.session_id).unwrap();
        let layout_text = app.message_layout.as_ref().unwrap().text.clone();
        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        assert!(app.thinking_active);
        assert_eq!(app.thinking_last_line, "模型正在思考");
        crate::ui::update_message_layout(&mut app, viewport);
        let rebuilds = app.output_layout_rebuild_count;
        let markdown_parses = app.markdown_parse_count;
        handle_agent_event(
            &mut app,
            AgentEvent::ReasoningDelta("第一行\n\n最新".into()),
        );
        assert_eq!(
            crate::ui::live_thinking_line_with_braille(&app, true),
            "⠋ 思考中  最新"
        );
        crate::ui::update_message_layout(&mut app, viewport);
        handle_agent_event(&mut app, AgentEvent::ReasoningDelta("一行".into()));

        assert_eq!(app.thinking_last_line, "最新一行");
        assert_eq!(
            crate::ui::live_thinking_line_with_braille(&app, true),
            "⠋ 思考中  最新一行"
        );
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.entries.len(), entries);
        assert_eq!(app.storage.load_messages(&app.session_id).unwrap(), stored);
        assert_eq!(app.message_layout.as_ref().unwrap().text, layout_text);
        assert!(
            !app.message_layout
                .as_ref()
                .unwrap()
                .text
                .contains("最新一行")
        );
        let layout = app.message_layout.as_ref().unwrap();
        let copied = layout
            .selected_text(OutputSelection {
                anchor: 0,
                active: layout.text.len(),
                dragging: false,
            })
            .unwrap();
        assert!(!copied.contains("最新一行"));
        assert_eq!(app.output_layout_rebuild_count, rebuilds);
        assert_eq!(app.markdown_parse_count, markdown_parses);
    }

    #[test]
    fn reasoning_without_newlines_keeps_utf8_safe_tail() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        let delta = format!("{}👩‍💻e\u{301}尾", "中文🙂".repeat(400));
        handle_agent_event(&mut app, AgentEvent::ReasoningDelta(delta));

        assert!(app.thinking_last_line.len() <= MAX_THINKING_LINE_BYTES);
        assert!(app.thinking_last_line.ends_with("👩‍💻e\u{301}尾"));
        assert!(!app.thinking_last_line.contains('\u{fffd}'));
    }

    #[test]
    fn reasoning_terminal_events_set_fixed_statuses() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);

        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        handle_agent_event(
            &mut app,
            AgentEvent::ReasoningDelta("正在分析工具结果".into()),
        );
        handle_agent_event(&mut app, AgentEvent::TextDelta("answer".into()));
        assert!(!app.thinking_active);
        assert_eq!(app.thinking_last_line, "正在分析工具结果");
        assert_eq!(app.thinking_result, ThinkingResult::Completed);
        assert_eq!(
            crate::ui::live_thinking_line(&app),
            "✓ 思考完成  正在分析工具结果"
        );

        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        handle_agent_event(&mut app, AgentEvent::ReasoningDelta("最后失败位置".into()));
        handle_agent_event(&mut app, AgentEvent::Failed("failed".into()));
        assert!(!app.thinking_active);
        assert_eq!(app.thinking_last_line, "最后失败位置");
        assert_eq!(app.thinking_result, ThinkingResult::Failed);
        assert_eq!(
            crate::ui::live_thinking_line(&app),
            "✗ 思考失败  最后失败位置"
        );

        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        handle_agent_event(&mut app, AgentEvent::ReasoningDelta("取消前内容".into()));
        handle_agent_event(&mut app, AgentEvent::Cancelled("cancelled".into()));
        assert!(!app.thinking_active);
        assert_eq!(app.thinking_last_line, "取消前内容");
        assert_eq!(app.thinking_result, ThinkingResult::Cancelled);
        assert_eq!(
            crate::ui::live_thinking_line(&app),
            "■ 思考已取消  取消前内容"
        );
    }

    #[test]
    fn thinking_animation_frames_loop_in_order_with_ascii_fallback() {
        assert_eq!(
            (0..11)
                .map(|frame| thinking_animation_glyph(frame, true))
                .collect::<String>(),
            "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⠋"
        );
        assert_eq!(
            (0..5)
                .map(|frame| thinking_animation_glyph(frame, false))
                .collect::<String>(),
            "|/-\\|"
        );
    }

    #[test]
    fn thinking_animation_does_not_rebuild_message_layout() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 4);
        crate::ui::update_message_layout(&mut app, viewport);
        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        crate::ui::update_message_layout(&mut app, viewport);
        let rebuilds = app.output_layout_rebuild_count;

        for _ in 0..10 {
            app.thinking_animation_frame = app.thinking_animation_frame.wrapping_add(1);
            crate::ui::update_message_layout(&mut app, viewport);
        }
        assert_eq!(app.output_layout_rebuild_count, rebuilds);
    }

    #[test]
    fn tool_rounds_reuse_one_live_thinking_row() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 40, 8);

        for round in 0..3 {
            handle_agent_event(&mut app, AgentEvent::ModelStreaming);
            handle_agent_event(
                &mut app,
                AgentEvent::ReasoningDelta(format!("第 {round} 轮")),
            );
            crate::ui::update_message_layout(&mut app, viewport);
            let layout = app.message_layout.as_ref().unwrap();
            assert_eq!(layout.live_thinking_rows, 1);
            assert_eq!(
                layout
                    .visual_lines
                    .iter()
                    .filter(|line| line.synthetic)
                    .count(),
                layout.live_thinking_rows
            );
            handle_agent_event(
                &mut app,
                AgentEvent::ToolStarted(ToolCall {
                    id: format!("call-{round}"),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path":"Cargo.toml"}),
                }),
            );
        }
    }

    #[test]
    fn live_thinking_row_is_in_layout_but_not_selectable() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 4);
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(
            app.message_layout
                .as_ref()
                .unwrap()
                .visual_lines
                .iter()
                .filter(|line| line.synthetic)
                .count(),
            0
        );

        app.push_entry(DisplayEntry {
            kind: DisplayKind::User,
            content: DisplayContent::Markdown("next request".into()),
        });
        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        crate::ui::update_message_layout(&mut app, viewport);
        let initial_rebuilds = app.output_layout_rebuild_count;
        let insertion = app.message_layout.as_ref().unwrap().live_thinking_before;
        handle_agent_event(&mut app, AgentEvent::ReasoningDelta("真实摘要".into()));
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.output_layout_rebuild_count, initial_rebuilds);
        let layout = app.message_layout.as_ref().unwrap();
        let live_row = layout
            .visual_lines
            .iter()
            .position(|line| line.synthetic)
            .unwrap();
        assert!(layout.position_at_visual_row(live_row, 0).is_none());

        handle_agent_event(&mut app, AgentEvent::TextDelta("answer".into()));
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.output_layout_rebuild_count, initial_rebuilds + 1);
        assert_eq!(
            app.message_layout.as_ref().unwrap().live_thinking_before,
            insertion
        );
        assert_eq!(
            app.message_layout
                .as_ref()
                .unwrap()
                .visual_lines
                .iter()
                .filter(|line| line.synthetic)
                .count(),
            1
        );
        assert_eq!(app.thinking_last_line, "真实摘要");
    }

    #[test]
    fn tool_click_toggle_rebuilds_once_per_toggle() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.push_entry(DisplayEntry {
            kind: DisplayKind::Tool,
            content: DisplayContent::Tool(ToolDisplay {
                call_id: "call-read".into(),
                name: "file_read".into(),
                arguments: serde_json::json!({"path":"src/app.rs"}),
                status: ToolDisplayStatus::Completed,
                result: Some("details".into()),
            }),
        });
        let viewport = Rect::new(0, 0, 30, 30);
        crate::ui::update_message_layout(&mut app, viewport);

        for expected_count in [2, 3] {
            let layout = app.message_layout.as_ref().unwrap();
            let target_row = layout
                .visual_lines
                .iter()
                .position(|line| {
                    line.interaction == Some(InteractionTarget::Tool("call-read".into()))
                })
                .unwrap()
                .saturating_sub(layout.scroll) as u16
                + layout.viewport.y;
            let down = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: target_row,
                modifiers: KeyModifiers::NONE,
            };
            let up = MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 1,
                row: target_row,
                modifiers: KeyModifiers::NONE,
            };
            assert!(handle_output_mouse(&mut app, down).redraw);
            assert!(handle_output_mouse(&mut app, up).redraw);
            assert!(app.output_layout_dirty);
            crate::ui::update_message_layout(&mut app, viewport);
            assert_eq!(app.output_layout_rebuild_count, expected_count);
        }
    }

    #[test]
    fn three_tools_render_as_one_group_and_keep_stable_expansion_ids() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.entries = ["file_read", "file_search", "file_write"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| DisplayEntry {
                kind: DisplayKind::Tool,
                content: DisplayContent::Tool(ToolDisplay {
                    call_id: format!("call-{index}"),
                    name: name.into(),
                    arguments: serde_json::json!({"path":"src/app.rs","query":"thinking"}),
                    status: ToolDisplayStatus::Completed,
                    result: Some("ok".into()),
                }),
            })
            .collect();
        app.invalidate_output_layout();
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 80, 20));
        let layout = app.message_layout.as_ref().unwrap();
        assert_eq!(
            layout.text.lines().filter(|line| *line == "工具").count(),
            1
        );
        assert_eq!(
            layout
                .lines
                .iter()
                .filter(|line| matches!(line.interaction, Some(InteractionTarget::Tool(_))))
                .count(),
            3
        );
        app.expanded_tools.insert("call-1".into());
        app.entries.insert(
            0,
            DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown("trim me".into()),
            },
        );
        trim_entries(&mut app.entries);
        assert!(app.expanded_tools.contains("call-1"));
    }

    #[test]
    fn tool_started_and_finished_update_one_display_entry() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let call = ToolCall {
            id: "merged-call".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"src/app.rs"}),
        };
        let initial_len = app.entries.len();
        handle_agent_event(&mut app, AgentEvent::ToolStarted(call.clone()));
        handle_agent_event(
            &mut app,
            AgentEvent::ToolFinished {
                call,
                result: "contents".into(),
            },
        );
        assert_eq!(app.entries.len(), initial_len + 1);
        assert!(matches!(app.entries.last().map(|entry| &entry.content),
            Some(DisplayContent::Tool(tool))
                if tool.status == ToolDisplayStatus::Completed
                    && tool.result.as_deref() == Some("contents")));
    }

    #[test]
    fn clicking_thinking_title_expands_and_collapses_live_rows() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        handle_agent_event(
            &mut app,
            AgentEvent::ReasoningDelta("第一行\n第二行".into()),
        );
        let viewport = Rect::new(0, 0, 80, 30);
        crate::ui::update_message_layout(&mut app, viewport);
        let thinking_row = app
            .message_layout
            .as_ref()
            .unwrap()
            .visual_lines
            .iter()
            .position(|line| line.interaction == Some(InteractionTarget::Thinking))
            .unwrap() as u16;
        let click = |kind| MouseEvent {
            kind,
            column: 1,
            row: thinking_row,
            modifiers: KeyModifiers::NONE,
        };
        handle_output_mouse(&mut app, click(MouseEventKind::Down(MouseButton::Left)));
        handle_output_mouse(&mut app, click(MouseEventKind::Up(MouseButton::Left)));
        assert!(app.thinking_expanded);
        crate::ui::update_message_layout(&mut app, viewport);
        assert!(app.message_layout.as_ref().unwrap().live_thinking_rows >= 3);

        handle_output_mouse(&mut app, click(MouseEventKind::Down(MouseButton::Left)));
        handle_output_mouse(&mut app, click(MouseEventKind::Up(MouseButton::Left)));
        assert!(!app.thinking_expanded);
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.message_layout.as_ref().unwrap().live_thinking_rows, 1);
    }

    #[test]
    fn dragging_from_tool_summary_does_not_toggle_it() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.push_entry(DisplayEntry {
            kind: DisplayKind::Tool,
            content: DisplayContent::Tool(ToolDisplay {
                call_id: "drag-tool".into(),
                name: "file_read".into(),
                arguments: serde_json::json!({"path":"src/app.rs"}),
                status: ToolDisplayStatus::Completed,
                result: Some("selectable result".into()),
            }),
        });
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 80, 30));
        let layout = app.message_layout.as_ref().unwrap();
        let row = layout
            .visual_lines
            .iter()
            .position(|line| line.interaction == Some(InteractionTarget::Tool("drag-tool".into())))
            .unwrap() as u16;
        handle_output_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row,
                modifiers: KeyModifiers::NONE,
            },
        );
        handle_output_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 10,
                row: row + 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        handle_output_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 10,
                row: row + 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert!(!app.expanded_tools.contains("drag-tool"));
    }

    #[tokio::test]
    async fn ctrl_o_no_longer_expands_tools() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let outcome = handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent {
                code: KeyCode::Char('o'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
        )
        .await
        .unwrap();
        assert!(!outcome.redraw);
        assert!(app.expanded_tools.is_empty());
    }

    #[test]
    fn thinking_expansion_and_buffer_limit_are_utf8_safe() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_agent_event(&mut app, AgentEvent::ModelStreaming);
        handle_agent_event(
            &mut app,
            AgentEvent::ReasoningDelta(format!("第一行\n{}尾🙂", "中文👩‍💻".repeat(20_000))),
        );
        assert!(app.thinking_buffer.len() <= MAX_THINKING_BUFFER_BYTES);
        assert!(app.thinking_buffer_truncated);
        assert!(app.thinking_buffer.ends_with("尾🙂"));
        assert!(app.thinking_buffer.is_char_boundary(0));
        app.thinking_expanded = true;
        let lines = crate::ui::live_thinking_line_with_braille(&app, true);
        assert!(lines.contains("[较早思考内容已截断]"));
        assert!(lines.contains("尾🙂"));
    }

    #[test]
    fn clear_and_session_activation_invalidate_layout() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 4);
        crate::ui::update_message_layout(&mut app, viewport);
        execute_command(&mut app, Command::Clear).unwrap();
        assert!(app.output_layout_dirty);
        assert!(app.message_layout.is_none());

        crate::ui::update_message_layout(&mut app, viewport);
        let session_id = app.storage.create_session(&app.workspace).unwrap();
        activate_session(&mut app, session_id).unwrap();
        assert!(app.output_layout_dirty);
        assert!(app.message_layout.is_none());
    }

    #[test]
    fn trimming_entries_releases_and_invalidates_layout() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 20, 4));
        app.entries.extend((0..1000).map(|_| DisplayEntry {
            kind: DisplayKind::System,
            content: DisplayContent::Markdown("x".into()),
        }));

        trim_app_entries(&mut app);
        assert_eq!(app.entries.len(), 1000);
        assert!(app.output_layout_dirty);
        assert!(app.message_layout.is_none());
    }

    #[tokio::test]
    async fn mouse_move_without_drag_and_key_release_do_not_redraw() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let moved = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!handle_terminal_event(&mut app, moved).await.unwrap().redraw);

        let released = Event::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert!(
            !handle_terminal_event(&mut app, released)
                .await
                .unwrap()
                .redraw
        );
    }

    fn wheel_event(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_wheel_moves_one_line_and_reuses_layout() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 1);
        crate::ui::update_message_layout(&mut app, viewport);
        let max_scroll = app.message_layout.as_ref().unwrap().max_scroll();
        assert!(max_scroll >= 3);
        assert_eq!(app.output_layout_rebuild_count, 1);

        handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollUp));
        assert_eq!(app.output_scroll_top, Some(max_scroll - 1));
        assert_eq!(app.message_scroll, 1);
        assert_eq!(app.output_layout_rebuild_count, 1);

        handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollUp));
        handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollUp));
        assert_eq!(app.output_scroll_top, Some(max_scroll - 3));
        assert_eq!(app.message_scroll, 3);
        assert_eq!(app.output_layout_rebuild_count, 1);
    }

    #[test]
    fn mouse_wheel_clamps_at_top_and_bottom_one_line_at_a_time() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 1);
        crate::ui::update_message_layout(&mut app, viewport);
        let max_scroll = app.message_layout.as_ref().unwrap().max_scroll();

        app.output_scroll_top = Some(0);
        app.follow_output = false;
        app.message_scroll = max_scroll;
        handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollUp));
        assert_eq!(app.output_scroll_top, Some(0));
        assert_eq!(app.message_scroll, max_scroll);

        handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollDown));
        assert_eq!(app.output_scroll_top, Some(1));
        assert_eq!(app.message_scroll, max_scroll - 1);

        app.output_scroll_top = Some(max_scroll);
        app.follow_output = false;
        app.message_scroll = 0;
        handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollDown));
        assert_eq!(app.output_scroll_top, None);
        assert!(app.follow_output);
        assert_eq!(app.message_scroll, 0);
        assert_eq!(app.output_layout_rebuild_count, 1);
    }
}
