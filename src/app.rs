use std::{
    collections::{HashMap, HashSet},
    future::pending,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
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
#[cfg(test)]
use tokio::sync::oneshot;
use tokio::sync::{Mutex, mpsc};

use crate::{
    agent::{AgentEvent, AgentRunner, ChildSessionProgress, ChildSessionStatus},
    commands::{self, AgentMode, Command, TodoCommand},
    config::{Config, ProviderPreset, ThinkingLevel, ThinkingProfile, thinking_profile},
    home::{self, HomeAction, HomeSelection, HomeState, RECENT_SESSION_LIMIT},
    input::InputBuffer,
    output::{EdgeScroll, InteractionTarget, OutputSelection},
    provider::{ConversationItem, OpenAiClient, Role, ToolCall, Usage},
    secrets,
    security::Workspace,
    session::{
        EventCtx, SessionRuntime, display_entries, estimate_context_tokens, trim_conversation,
    },
    settings::{SettingsField, SettingsForm, SettingsState},
    storage::{SessionSummary, Storage},
    tools::ToolRegistry,
    ui,
};

const MOUSE_WHEEL_SCROLL_LINES: isize = 1;
const DEFERRED_REDRAW_INTERVAL: Duration = Duration::from_millis(16);

pub(crate) use crate::model::ThinkingResult;
pub use crate::model::{
    AgentPhase, ApprovalAction, DisplayContent, DisplayEntry, DisplayKind, ModelPhase,
    PendingApproval, ThinkingDisplay, TodoDisplay, TodoStatus, TodoTask, ToolDisplay,
    ToolDisplayStatus,
};

#[derive(Clone, Debug)]
pub struct CommandPaletteState {
    pub query: String,
    pub selected: usize,
}

pub struct App {
    pub workspace: PathBuf,
    pub input: InputBuffer,
    pub context_meter_enabled: bool,
    pub settings: Option<SettingsState>,
    pub settings_rect: Option<Rect>,
    pub palette: Option<CommandPaletteState>,
    pub thinking_menu_open: bool,
    pub thinking_control_rect: Option<Rect>,
    pub thinking_menu_rect: Option<Rect>,
    pub session_panel_rect: Option<Rect>,
    pub input_mode_rect: Option<Rect>,
    pub provider_control_rect: Option<Rect>,
    pub model_control_rect: Option<Rect>,
    pub provider_menu_open: bool,
    pub provider_menu_rect: Option<Rect>,
    pub provider_menu_selected: usize,
    pub model_menu_open: bool,
    pub model_menu_rect: Option<Rect>,
    pub model_menu_selected: usize,
    pub force_full_redraw: bool,
    pub mouse_press_target: Option<InteractionTarget>,
    pub mouse_press_position: Option<(u16, u16)>,
    pub mouse_dragged: bool,
    pub layout_restore_anchor: Option<(InteractionTarget, usize)>,
    pub file_suggestions: Vec<String>,
    pub file_selected: usize,
    pub sessions: Vec<SessionSummary>,
    pub expanded_sessions: HashSet<String>,
    pub child_status: HashMap<String, ChildSessionProgress>,
    pub child_batches: HashMap<String, HashSet<String>>,
    storage: Storage,
    config: Config,
    registry: Arc<ToolRegistry>,
    approval_lock: Arc<Mutex<()>>,
    active_secret: Option<(ProviderPreset, String)>,
    active_session: String,
    pub current: SessionRuntime,
    background: HashMap<String, SessionRuntime>,
    router_tx: mpsc::Sender<RoutedEvent>,
    router_rx: mpsc::Receiver<RoutedEvent>,
    should_quit: bool,
}

/// An agent event tagged with the session it belongs to, so a single channel
/// can route events to any background session in O(1).
struct RoutedEvent {
    session_id: String,
    event: AgentEvent,
}

pub async fn run(workspace_path: PathBuf, mut config: Config) -> Result<()> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("cannot create data directory {}", config.data_dir.display()))?;
    let storage = Storage::open(&config.data_dir.join("agent.db"))?;
    // Only the active provider may touch the OS keyring during startup. Each
    // provider is a separate credential on macOS, so enumerating all presets
    // can produce one authorization dialog per saved key. Environment-backed
    // keys are safe to preload without interacting with any platform backend.
    secrets::preload_environment_keys();
    let _ = secrets::api_key_cached(config.provider.preset);
    let recent_sessions = storage.list_recent_sessions(&workspace_path, RECENT_SESSION_LIMIT)?;
    let mut home = HomeState::new(
        &workspace_path,
        config.provider.clone(),
        config.providers.clone(),
        recent_sessions,
    );

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
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                PopKeyboardEnhancementFlags,
                LeaveAlternateScreen,
                DisableMouseCapture,
                DisableBracketedPaste
            );
            return Err(error.into());
        }
    };
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

    let result = async {
        let action = home_event_loop(&mut terminal, &mut home).await?;
        if action == HomeAction::Quit {
            return Ok(());
        }
        let selection = matches!(action, HomeAction::StartNew(_)).then(|| home.selection());
        home.set_loading();
        terminal.draw(|frame| home::draw(frame, &mut home))?;
        let Some((session_id, first_prompt)) =
            resolve_home_action(&storage, &workspace_path, action)?
        else {
            return Ok(());
        };
        if let Some(selection) = selection {
            if selection.provider.preset != config.provider.preset {
                let _ = secrets::api_key_cached(selection.provider.preset);
            }
            apply_home_selection(&mut config, &storage, &session_id, selection)?;
        }
        drop(home);
        let mut app = build_app(workspace_path, config, storage, session_id).await?;
        if let Some(prompt) = first_prompt {
            app.input.set(prompt);
            submit_input(&mut app)?;
        }
        event_loop(&mut terminal, &mut app).await
    }
    .await;

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

fn apply_home_selection(
    config: &mut Config,
    storage: &Storage,
    session_id: &str,
    selection: HomeSelection,
) -> Result<()> {
    config.provider = selection.provider;
    config.upsert_provider(config.provider.clone());
    let _ = config.save();
    storage.set_session_mode(session_id, selection.mode.as_str())?;
    Ok(())
}

fn resolve_home_action(
    storage: &Storage,
    workspace: &Path,
    action: HomeAction,
) -> Result<Option<(String, Option<String>)>> {
    match action {
        HomeAction::StartNew(prompt) => {
            Ok(Some((storage.create_session(workspace)?, Some(prompt))))
        }
        HomeAction::Resume(session_id) => Ok(Some((session_id, None))),
        HomeAction::Quit => Ok(None),
    }
}

async fn build_app(
    workspace_path: PathBuf,
    config: Config,
    storage: Storage,
    session_id: String,
) -> Result<App> {
    let sessions = storage.list_sessions(&workspace_path)?;
    let workspace = Workspace::new(&workspace_path)?;
    let registry = Arc::new(ToolRegistry::new(
        workspace,
        config.runtime.clone(),
        config.security.allow_private_networks,
    ));
    registry.set_permission_rules(config.permissions.tools.clone());
    registry.set_external_config(config.browser.clone(), config.mcp_servers.clone());
    let _ = registry.initialize_mcp().await;
    let (router_tx, router_rx) = mpsc::channel(256);
    let approval_lock = Arc::new(Mutex::new(()));
    // A restored child session may own a different provider. This is an
    // explicit resume action, so unlock only that one additional credential.
    if let Some(provider_config) = storage
        .session_provider_model(&session_id)
        .ok()
        .and_then(|(provider_id, model)| session_provider_config(&config, &provider_id, &model))
        && provider_config.preset != config.provider.preset
    {
        let _ = secrets::api_key_cached(provider_config.preset);
    }
    let (active_secret, initial_status) = match secrets::api_key_cached_only(config.provider.preset)
    {
        Ok(api_key) => (
            Some((config.provider.preset, api_key)),
            format!(
                "Ready | {} | {}",
                config.provider.preset.label(),
                config.provider.model
            ),
        ),
        Err(secrets::SecretError::Missing(_)) => (None, "需要配置提供商".into()),
        Err(error) => (
            None,
            format!(
                "系统密钥环读取失败：{}",
                secrets::redact(&error.to_string())
            ),
        ),
    };
    let initial_mode = storage
        .session_mode(&session_id)
        .ok()
        .and_then(|value| AgentMode::parse(&value))
        .unwrap_or_default();
    registry.set_mode(initial_mode);
    let mut runtime = build_runtime(
        &storage,
        &config,
        &registry,
        &router_tx,
        &approval_lock,
        active_secret.as_ref(),
        &session_id,
    );
    runtime.status = initial_status;
    Ok(App {
        workspace: workspace_path,
        input: InputBuffer::new(),
        context_meter_enabled: config.ui.context_meter,
        settings: None,
        settings_rect: None,
        palette: None,
        thinking_menu_open: false,
        thinking_control_rect: None,
        thinking_menu_rect: None,
        session_panel_rect: None,
        input_mode_rect: None,
        provider_control_rect: None,
        model_control_rect: None,
        provider_menu_open: false,
        provider_menu_rect: None,
        provider_menu_selected: 0,
        model_menu_open: false,
        model_menu_rect: None,
        model_menu_selected: 0,
        force_full_redraw: false,
        mouse_press_target: None,
        mouse_press_position: None,
        mouse_dragged: false,
        layout_restore_anchor: None,
        file_suggestions: Vec::new(),
        file_selected: 0,
        sessions,
        expanded_sessions: HashSet::new(),
        child_status: HashMap::new(),
        child_batches: HashMap::new(),
        storage,
        config,
        registry,
        approval_lock,
        active_secret,
        active_session: session_id,
        current: runtime,
        background: HashMap::new(),
        router_tx,
        router_rx,
        should_quit: false,
    })
}

async fn home_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut HomeState,
) -> Result<HomeAction> {
    let mut terminal_events = EventStream::new();
    terminal.draw(|frame| home::draw(frame, state))?;
    loop {
        let Some(event) = terminal_events.next().await else {
            return Ok(HomeAction::Quit);
        };
        let outcome = state.handle_event(event?);
        if let Some(action) = outcome.action {
            return Ok(action);
        }
        if outcome.redraw {
            terminal.draw(|frame| home::draw(frame, state))?;
        }
    }
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut edge_scroll_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let mut thinking_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let mut deferred_redraw_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    terminal.draw(|frame| ui::draw(frame, app))?;

    while !app.should_quit {
        if app
            .current
            .output_selection
            .is_some_and(|selection| selection.dragging)
            && app.current.edge_scroll.direction != 0
            && edge_scroll_timer.is_none()
        {
            edge_scroll_timer = Some(Box::pin(tokio::time::sleep(
                std::time::Duration::from_millis(80),
            )));
        }
        if !app
            .current
            .output_selection
            .is_some_and(|selection| selection.dragging)
            || app.current.edge_scroll.direction == 0
        {
            edge_scroll_timer = None;
        }
        if app.current.thinking_active && thinking_timer.is_none() {
            thinking_timer = Some(Box::pin(tokio::time::sleep(
                std::time::Duration::from_millis(100),
            )));
        }
        if !app.current.thinking_active {
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
        let deferred_redraw_tick = async {
            if let Some(timer) = deferred_redraw_timer.as_mut() {
                timer.await;
            } else {
                pending::<()>().await;
            }
        };
        let mut redraw = false;
        tokio::select! {
            _ = deferred_redraw_tick => {
                deferred_redraw_timer = None;
                redraw = true;
            }
            _ = edge_scroll_tick => {
                edge_scroll_timer = None;
                auto_scroll_selection(app);
                redraw = true;
            }
            _ = thinking_tick => {
                thinking_timer = None;
                app.current.thinking_animation_frame = app.current.thinking_animation_frame.wrapping_add(1);
                redraw = true;
            }
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(event)) => {
                        let coalesce = should_coalesce_terminal_redraw(&event);
                        let outcome = handle_terminal_event(app, event).await?;
                        redraw = outcome.redraw;
                        if redraw && coalesce {
                            schedule_deferred_redraw(&mut deferred_redraw_timer);
                            redraw = false;
                        }
                        if let Some(sequence) = outcome.osc52 {
                            execute!(terminal.backend_mut(), Print(sequence))?;
                        }
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => break,
                }
            }
            agent_event = app.router_rx.recv() => {
                if let Some(routed) = agent_event {
                    let coalesce = should_coalesce_stream_redraw(&app.active_session, &routed);
                    redraw = handle_routed_event(app, routed);
                    if redraw && coalesce {
                        schedule_deferred_redraw(&mut deferred_redraw_timer);
                        redraw = false;
                    }
                }
            }
        }
        if redraw {
            // An immediate frame includes any scroll and stream updates accumulated so far.
            deferred_redraw_timer = None;
            if app.force_full_redraw {
                terminal.clear()?;
                app.force_full_redraw = false;
            }
            terminal.draw(|frame| ui::draw(frame, app))?;
        }
    }
    app.current.shutdown();
    for runtime in app.background.values_mut() {
        runtime.shutdown();
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
        if app.settings.is_some() {
            return Ok(if paste_text_into_settings(app, text) {
                EventOutcome::redraw()
            } else {
                EventOutcome::default()
            });
        }
        if !app.current.busy && app.palette.is_none() {
            app.input.insert_str(text);
            update_file_suggestions(app);
            return Ok(EventOutcome::redraw());
        }
        return Ok(EventOutcome::default());
    }
    if let Event::Mouse(mouse) = event {
        if let Some(outcome) = handle_settings_mouse(app, mouse)? {
            return Ok(outcome);
        }
        if let Some(outcome) = handle_thinking_mouse(app, mouse)? {
            return Ok(outcome);
        }
        if let Some(outcome) = handle_provider_mouse(app, mouse)? {
            return Ok(outcome);
        }
        if let Some(outcome) = handle_model_mouse(app, mouse)? {
            return Ok(outcome);
        }
        if let Some(outcome) = handle_navigation_mouse(app, mouse)? {
            return Ok(outcome);
        }
        if output_mouse_event_allowed(
            mouse.kind,
            app.settings.is_some(),
            app.palette.is_some(),
            app.has_pending_approval(),
        ) {
            return Ok(handle_output_mouse(app, mouse));
        }
        return Ok(EventOutcome::default());
    }
    if matches!(event, Event::Resize(_, _)) {
        if app.has_pending_approval()
            || app.settings.is_some()
            || app.palette.is_some()
            || app.thinking_menu_open
            || app.provider_menu_open
            || app.model_menu_open
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
    if app.has_pending_approval() {
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
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => resolve_approval(app, false),
            _ => {}
        }
        return Ok(EventOutcome {
            redraw,
            osc52: None,
        });
    }
    if app.settings.is_some() {
        let redraw = settings_key_handled(key.code, key.modifiers);
        handle_settings_key(app, key.code, key.modifiers);
        return Ok(EventOutcome {
            redraw,
            osc52: None,
        });
    }
    if app.provider_menu_open {
        let redraw = provider_menu_key_handled(key.code);
        handle_provider_menu_key(app, key.code)?;
        return Ok(EventOutcome {
            redraw,
            osc52: None,
        });
    }
    if app.model_menu_open {
        let redraw = model_menu_key_handled(key.code);
        handle_model_menu_key(app, key.code)?;
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
    let redraw = match key.code {
        KeyCode::Char('p' | 'x')
            if key.modifiers.contains(KeyModifiers::CONTROL) && !app.current.busy =>
        {
            open_palette(app);
            true
        }
        KeyCode::PageUp if !app.current.busy => app.current.scroll_messages(5),
        KeyCode::PageDown if !app.current.busy => app.current.scroll_messages(-5),
        KeyCode::Up if !app.current.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.current.scroll_messages(3)
        }
        KeyCode::Down if !app.current.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.current.scroll_messages(-3)
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.current.scroll_to_bottom();
            true
        }
        KeyCode::PageUp if !app.current.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.current.scroll_messages(5)
        }
        KeyCode::PageDown if !app.current.busy && key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.current.scroll_messages(-5)
        }
        KeyCode::Char('s')
            if key.modifiers.contains(KeyModifiers::CONTROL) && !app.current.busy =>
        {
            open_settings(app);
            true
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            create_session(app)?;
            true
        }
        KeyCode::Up if session_switch_direction(&key) == Some(-1) => {
            switch_session(app, -1)?;
            true
        }
        KeyCode::Down if session_switch_direction(&key) == Some(1) => {
            switch_session(app, 1)?;
            true
        }
        KeyCode::Esc => {
            if app.current.active_task.is_some() {
                cancel_active_request(app);
            }
            true
        }
        KeyCode::Tab if !app.current.busy && !app.file_suggestions.is_empty() => {
            apply_file_completion(app);
            true
        }
        KeyCode::Enter if !app.current.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.insert('\n');
            app.file_suggestions.clear();
            true
        }
        KeyCode::Char('j')
            if !app.current.busy && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.input.insert('\n');
            app.file_suggestions.clear();
            true
        }
        KeyCode::Enter if !app.current.busy => {
            submit_input(app)?;
            true
        }
        KeyCode::Backspace if !app.current.busy => {
            app.input.backspace();
            update_file_suggestions(app);
            true
        }
        KeyCode::Delete if !app.current.busy => {
            app.input.delete();
            update_file_suggestions(app);
            true
        }
        KeyCode::Left if !app.current.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.select_left();
            true
        }
        KeyCode::Right if !app.current.busy && key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.select_right();
            true
        }
        KeyCode::Char('a')
            if !app.current.busy && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.input.select_all();
            true
        }
        KeyCode::Left if !app.current.busy => {
            app.input.move_left();
            true
        }
        KeyCode::Right if !app.current.busy => {
            app.input.move_right();
            true
        }
        KeyCode::Home if !app.current.busy => {
            app.input.move_home();
            true
        }
        KeyCode::End if !app.current.busy => {
            app.input.move_end();
            true
        }
        KeyCode::Up
            if !app.current.busy
                && key.modifiers.is_empty()
                && !app.file_suggestions.is_empty() =>
        {
            app.file_selected = app.file_selected.saturating_sub(1);
            true
        }
        KeyCode::Down
            if !app.current.busy
                && key.modifiers.is_empty()
                && !app.file_suggestions.is_empty() =>
        {
            app.file_selected =
                (app.file_selected + 1).min(app.file_suggestions.len().saturating_sub(1));
            true
        }
        KeyCode::Up if !app.current.busy && key.modifiers.is_empty() => {
            if app.input.is_empty() {
                app.input.history_previous();
            } else {
                app.input.move_up();
            }
            true
        }
        KeyCode::Down if !app.current.busy && key.modifiers.is_empty() => {
            if app.input.is_empty() {
                app.input.history_next();
            } else {
                app.input.move_down();
            }
            true
        }
        KeyCode::Char('w')
            if !app.current.busy && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.input.delete_word_left();
            true
        }
        KeyCode::Char('u')
            if !app.current.busy && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.input.clear();
            true
        }
        KeyCode::Char(character)
            if !app.current.busy
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

fn handle_settings_mouse(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
) -> Result<Option<EventOutcome>> {
    let Some(settings) = app.settings.as_mut() else {
        return Ok(None);
    };
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return Ok(Some(EventOutcome::default()));
    }
    let Some(rect) = app.settings_rect else {
        return Ok(Some(EventOutcome::default()));
    };
    let inner = ratatui::widgets::Block::bordered().inner(rect);
    if !point_in_rect(mouse.column, mouse.row, inner) {
        return Ok(Some(EventOutcome::default()));
    }
    let relative_row = mouse.row.saturating_sub(inner.y) as usize;
    match settings {
        SettingsState::List(list) => {
            let profile_start = 2usize;
            if relative_row >= profile_start && relative_row < profile_start + list.providers.len()
            {
                list.selected = relative_row - profile_start;
                open_selected_profile(app);
            } else if relative_row == profile_start + list.providers.len() + 1 {
                list.selected = list.providers.len();
                open_template_picker(app);
            }
        }
        SettingsState::Templates(templates) => {
            let start = 2usize;
            if relative_row >= start && relative_row < start + templates.presets.len() {
                templates.selected = relative_row - start;
                open_selected_template(app);
            }
        }
        SettingsState::Form(_) => {}
    }
    Ok(Some(EventOutcome::redraw()))
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
    if !app.current.busy
        && !app.has_pending_approval()
        && app
            .thinking_control_rect
            .is_some_and(|rect| point_in_rect(mouse.column, mouse.row, rect))
    {
        app.model_menu_open = false;
        app.model_menu_rect = None;
        app.provider_menu_open = false;
        app.provider_menu_rect = None;
        app.thinking_menu_open = true;
        return Ok(Some(EventOutcome::redraw()));
    }
    Ok(None)
}

pub(crate) fn model_choices(app: &App) -> Vec<String> {
    let mut choices = app
        .config
        .provider
        .preset
        .selectable_models()
        .iter()
        .map(|model| (*model).to_owned())
        .collect::<Vec<_>>();
    if choices.is_empty() {
        choices.push(app.config.provider.model.clone());
    } else if !choices
        .iter()
        .any(|model| model == &app.config.provider.model)
    {
        choices.insert(0, app.config.provider.model.clone());
    }
    choices
}

pub(crate) fn provider_choices(app: &App) -> Vec<ProviderPreset> {
    let mut choices = app
        .config
        .providers
        .iter()
        .map(|provider| provider.preset)
        .collect::<Vec<_>>();
    if !choices.contains(&app.config.provider.preset) {
        choices.insert(0, app.config.provider.preset);
    }
    choices
}

fn apply_provider_choice(app: &mut App, preset: ProviderPreset) -> Result<()> {
    if preset == app.config.provider.preset {
        return Ok(());
    }
    let Some(provider) = app.config.provider_for(preset) else {
        app.current.status = "供应商连接不存在".into();
        return Ok(());
    };
    let api_key = app
        .active_secret
        .as_ref()
        .filter(|(active, _)| *active == preset)
        .map(|(_, key)| key.clone())
        .or_else(|| secrets::api_key_cached(preset).ok());
    let Some(api_key) = api_key else {
        app.current.status = format!("{} 的 API Key 不可用，请在供应商设置中补充", preset.label());
        return Ok(());
    };

    app.storage.clear_response_id(&app.current.session_id)?;
    app.config.provider = provider;
    app.active_secret = Some((preset, api_key));
    app.current.context_limit_tokens = app.config.provider.resolved_context_window_tokens();
    rebuild_runner(app)?;
    app.current.status = match app.config.save() {
        Ok(()) => format!(
            "已切换到 {} · {}",
            preset.label(),
            app.config.provider.model
        ),
        Err(error) => format!(
            "供应商已切换；配置保存失败：{}",
            secrets::redact(&error.to_string())
        ),
    };
    Ok(())
}

fn handle_provider_mouse(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
) -> Result<Option<EventOutcome>> {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return Ok(app.provider_menu_open.then(EventOutcome::default));
    }
    if app.provider_menu_open {
        let selected = app
            .provider_menu_rect
            .filter(|rect| point_in_rect(mouse.column, mouse.row, *rect))
            .and_then(|rect| {
                let inner = ratatui::widgets::Block::bordered().inner(rect);
                if !point_in_rect(mouse.column, mouse.row, inner) {
                    return None;
                }
                let index = mouse.row.saturating_sub(inner.y) as usize;
                provider_choices(app).get(index).copied()
            });
        app.provider_menu_open = false;
        app.provider_menu_rect = None;
        if let Some(preset) = selected {
            apply_provider_choice(app, preset)?;
        }
        return Ok(Some(EventOutcome::redraw()));
    }
    if app.current.busy
        || app.has_pending_approval()
        || app.settings.is_some()
        || app.palette.is_some()
    {
        return Ok(None);
    }
    if app
        .provider_control_rect
        .is_some_and(|rect| point_in_rect(mouse.column, mouse.row, rect))
    {
        app.thinking_menu_open = false;
        app.thinking_menu_rect = None;
        app.model_menu_open = false;
        app.model_menu_rect = None;
        let choices = provider_choices(app);
        app.provider_menu_selected = choices
            .iter()
            .position(|preset| *preset == app.config.provider.preset)
            .unwrap_or(0);
        app.provider_menu_open = true;
        return Ok(Some(EventOutcome::redraw()));
    }
    Ok(None)
}

fn provider_menu_key_handled(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Esc | KeyCode::Up | KeyCode::Down | KeyCode::Enter
    )
}

fn handle_provider_menu_key(app: &mut App, code: KeyCode) -> Result<()> {
    let choices = provider_choices(app);
    if choices.is_empty() {
        app.provider_menu_open = false;
        return Ok(());
    }
    match code {
        KeyCode::Esc => {
            app.provider_menu_open = false;
            app.provider_menu_rect = None;
        }
        KeyCode::Up => {
            app.provider_menu_selected =
                (app.provider_menu_selected + choices.len() - 1) % choices.len();
        }
        KeyCode::Down => {
            app.provider_menu_selected = (app.provider_menu_selected + 1) % choices.len();
        }
        KeyCode::Enter => {
            apply_provider_choice(app, choices[app.provider_menu_selected])?;
            app.provider_menu_open = false;
            app.provider_menu_rect = None;
        }
        _ => {}
    }
    Ok(())
}

fn apply_model_choice(app: &mut App, model: String) -> Result<()> {
    if model.trim().is_empty() {
        return Ok(());
    }
    app.config.provider.model = model;
    app.config.provider.normalize_thinking();
    app.config.upsert_provider(app.config.provider.clone());
    app.current.context_limit_tokens = app.config.provider.resolved_context_window_tokens();
    app.storage.clear_response_id(&app.current.session_id)?;
    rebuild_runner(app)?;
    let status = match app.config.save() {
        Ok(()) => format!("模型已设置为 {}", app.config.provider.model),
        Err(error) => format!(
            "模型已更新；配置保存失败：{}",
            secrets::redact(&error.to_string())
        ),
    };
    app.current.status = status;
    Ok(())
}

fn handle_model_mouse(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
) -> Result<Option<EventOutcome>> {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return Ok(app.model_menu_open.then(EventOutcome::default));
    }
    if app.model_menu_open {
        let Some(rect) = app.model_menu_rect else {
            app.model_menu_open = false;
            return Ok(Some(EventOutcome::redraw()));
        };
        if !point_in_rect(mouse.column, mouse.row, rect) {
            app.model_menu_open = false;
            app.model_menu_rect = None;
            return Ok(Some(EventOutcome::redraw()));
        }
        let inner = ratatui::widgets::Block::bordered().inner(rect);
        let choices = model_choices(app);
        let visible = inner.height as usize;
        let scroll = app
            .model_menu_selected
            .saturating_sub(visible.saturating_sub(1));
        let index = scroll + mouse.row.saturating_sub(inner.y) as usize;
        if let Some(model) = choices.get(index).cloned() {
            apply_model_choice(app, model)?;
        }
        app.model_menu_open = false;
        app.model_menu_rect = None;
        return Ok(Some(EventOutcome::redraw()));
    }
    if app.current.busy
        || app.has_pending_approval()
        || app.settings.is_some()
        || app.palette.is_some()
    {
        return Ok(None);
    }
    if app
        .model_control_rect
        .is_some_and(|rect| point_in_rect(mouse.column, mouse.row, rect))
    {
        app.thinking_menu_open = false;
        app.thinking_menu_rect = None;
        app.provider_menu_open = false;
        app.provider_menu_rect = None;
        let choices = model_choices(app);
        app.model_menu_selected = choices
            .iter()
            .position(|model| model == &app.config.provider.model)
            .unwrap_or(0);
        app.model_menu_open = true;
        return Ok(Some(EventOutcome::redraw()));
    }
    Ok(None)
}

fn model_menu_key_handled(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Esc | KeyCode::Up | KeyCode::Down | KeyCode::Enter
    )
}

fn handle_model_menu_key(app: &mut App, code: KeyCode) -> Result<()> {
    let choices = model_choices(app);
    if choices.is_empty() {
        app.model_menu_open = false;
        return Ok(());
    }
    match code {
        KeyCode::Esc => {
            app.model_menu_open = false;
            app.model_menu_rect = None;
        }
        KeyCode::Up => {
            app.model_menu_selected = (app.model_menu_selected + choices.len() - 1) % choices.len();
        }
        KeyCode::Down => {
            app.model_menu_selected = (app.model_menu_selected + 1) % choices.len();
        }
        KeyCode::Enter => {
            let model = choices[app.model_menu_selected].clone();
            apply_model_choice(app, model)?;
            app.model_menu_open = false;
            app.model_menu_rect = None;
        }
        _ => {}
    }
    Ok(())
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
    app.current.status = match app.config.save() {
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

fn handle_navigation_mouse(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
) -> Result<Option<EventOutcome>> {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left)
        || app.settings.is_some()
        || app.palette.is_some()
        || app.has_pending_approval()
    {
        return Ok(None);
    }
    if let Some(area) = app.session_panel_rect {
        let rows = ui::flatten_session_tree(&app.sessions, &app.expanded_sessions);
        let current = rows
            .iter()
            .position(|row| row.id == app.current.session_id)
            .unwrap_or(0);
        if let Some(index) =
            ui::session_index_at(area, mouse.column, mouse.row, rows.len(), current)
        {
            let row = &rows[index];
            if row.has_children {
                if !app.expanded_sessions.insert(row.id.clone()) {
                    app.expanded_sessions.remove(&row.id);
                }
                return Ok(Some(EventOutcome::redraw()));
            }
            if row.id == app.current.session_id {
                return Ok(Some(EventOutcome::redraw()));
            }
            activate_session(app, row.id.clone())?;
            return Ok(Some(EventOutcome::redraw()));
        }
    }
    if let Some(rect) = app.input_mode_rect
        && point_in_rect(mouse.column, mouse.row, rect)
    {
        if app.current.busy {
            app.current.status = "请求运行中，无法切换模式".into();
            return Ok(Some(EventOutcome::redraw()));
        }
        switch_mode(app, next_mode(app.current.mode))?;
        return Ok(Some(EventOutcome::redraw()));
    }
    Ok(None)
}

fn handle_output_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) -> EventOutcome {
    match mouse.kind {
        MouseEventKind::ScrollUp => EventOutcome {
            redraw: app.current.scroll_messages(MOUSE_WHEEL_SCROLL_LINES),
            osc52: None,
        },
        MouseEventKind::ScrollDown => EventOutcome {
            redraw: app.current.scroll_messages(-MOUSE_WHEEL_SCROLL_LINES),
            osc52: None,
        },
        MouseEventKind::Down(MouseButton::Left) => {
            app.mouse_dragged = false;
            app.mouse_press_position = Some((mouse.column, mouse.row));
            app.mouse_press_target = app
                .current
                .message_layout
                .as_ref()
                .and_then(|layout| layout.interaction_at(mouse.column, mouse.row));
            if app.mouse_press_target.is_some() {
                app.current.clear_output_selection();
                app.current.edge_scroll = EdgeScroll::default();
                return EventOutcome::redraw();
            }
            let Some(offset) = app
                .current
                .message_layout
                .as_ref()
                .and_then(|layout| layout.hit_test(mouse.column, mouse.row))
            else {
                app.current.clear_output_selection();
                return EventOutcome::redraw();
            };
            if app.current.follow_output {
                app.current.output_scroll_top = app
                    .current
                    .message_layout
                    .as_ref()
                    .map(|layout| layout.scroll);
                if let Some(layout) = &app.current.message_layout {
                    app.current.message_scroll = layout.max_scroll().saturating_sub(layout.scroll);
                }
            }
            app.current.follow_output = false;
            app.current.output_selection = Some(OutputSelection::new(offset));
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
                            .current
                            .message_layout
                            .as_ref()
                            .and_then(|layout| layout.hit_test(column, row))
                    {
                        app.current.output_selection = Some(OutputSelection::new(offset));
                    }
                    update_drag_position(app, mouse.column, mouse.row);
                    return EventOutcome::redraw();
                }
                return EventOutcome::default();
            }
            if app
                .current
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
            app.current.edge_scroll = EdgeScroll::default();
            let pressed_target = app.mouse_press_target.take();
            app.mouse_press_position = None;
            if let Some(target) = pressed_target {
                let released_target = app
                    .current
                    .message_layout
                    .as_ref()
                    .and_then(|layout| layout.interaction_at(mouse.column, mouse.row));
                if !app.mouse_dragged && released_target.as_ref() == Some(&target) {
                    if !app.current.follow_output {
                        app.layout_restore_anchor =
                            app.current.message_layout.as_ref().and_then(|layout| {
                                layout
                                    .visual_lines
                                    .iter()
                                    .position(|line| line.interaction.as_ref() == Some(&target))
                                    .map(|visual_row| {
                                        (target.clone(), visual_row.saturating_sub(layout.scroll))
                                    })
                            });
                    }
                    let live_thinking_target = matches!(&target, InteractionTarget::Thinking);
                    match target {
                        InteractionTarget::Tool(call_id) => {
                            if !app.current.expanded_tools.insert(call_id.clone()) {
                                app.current.expanded_tools.remove(&call_id);
                            }
                        }
                        InteractionTarget::Thinking => {
                            app.current.thinking_expanded = !app.current.thinking_expanded;
                        }
                        InteractionTarget::ThinkingSummary(id) => {
                            if !app.current.expanded_thinking.insert(id.clone()) {
                                app.current.expanded_thinking.remove(&id);
                            }
                        }
                        InteractionTarget::Todo(task_id) => {
                            let mut tasks = app.current.todos.clone();
                            if let Some(task) = tasks.iter_mut().find(|task| task.id == task_id) {
                                task.status = task.status.next();
                                task.updated_at = chrono::Utc::now().to_rfc3339();
                                match app.storage.replace_tasks(&app.current.session_id, &tasks) {
                                    Ok(()) => {
                                        app.current.todos = tasks;
                                    }
                                    Err(error) => {
                                        app.current.status = format!("任务更新失败：{error}");
                                    }
                                }
                            }
                        }
                    }
                    if !live_thinking_target {
                        app.current.invalidate_output_layout();
                    }
                }
                app.mouse_dragged = false;
                return EventOutcome::redraw();
            }
            app.mouse_dragged = false;
            let Some(mut selection) = app.current.output_selection else {
                return EventOutcome::default();
            };
            selection.dragging = false;
            let Some((start, end)) = selection.range() else {
                app.current.output_selection = None;
                return EventOutcome::redraw();
            };
            app.current.output_selection = Some(selection);
            let Some(text) = app
                .current
                .message_layout
                .as_ref()
                .and_then(|layout| layout.text.get(start..end))
                .map(str::to_owned)
            else {
                app.current.status = "复制失败：选区位置已失效".into();
                return EventOutcome::redraw();
            };
            match crate::clipboard::copy_text(&text) {
                crate::clipboard::CopyResult::Native => {
                    app.current.status = "系统剪贴板已复制".into();
                    EventOutcome::redraw()
                }
                crate::clipboard::CopyResult::Osc52Requested(sequence) => {
                    app.current.status = "已向终端发送复制请求".into();
                    EventOutcome {
                        redraw: true,
                        osc52: Some(sequence),
                    }
                }
                crate::clipboard::CopyResult::Error(error) => {
                    app.current.status = format!("复制失败：{error}");
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
    let paste_shortcut = code == KeyCode::Char('v')
        && modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::CONTROL | KeyModifiers::META);
    paste_shortcut
        || matches!(
            code,
            KeyCode::Esc
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Enter
        )
        || (code == KeyCode::Char('d') && modifiers.contains(KeyModifiers::CONTROL))
        || matches!(code, KeyCode::Char(_) if !modifiers.contains(KeyModifiers::CONTROL))
}

fn update_drag_position(app: &mut App, column: u16, row: u16) {
    update_edge_scroll(app, column, row);
    let Some(offset) = app.current.message_layout.as_ref().and_then(|layout| {
        let clamped_row = row
            .max(layout.viewport.y)
            .min(layout.viewport.bottom().saturating_sub(1));
        layout.hit_test(column, clamped_row)
    }) else {
        return;
    };
    if let Some(selection) = &mut app.current.output_selection {
        selection.active = offset;
    }
}

fn update_edge_scroll(app: &mut App, column: u16, row: u16) {
    let Some(layout) = &app.current.message_layout else {
        return;
    };
    let direction = edge_scroll_direction(row, layout.viewport);
    app.current.edge_scroll = EdgeScroll { direction, column };
}

fn auto_scroll_selection(app: &mut App) {
    let direction = app.current.edge_scroll.direction;
    if direction == 0
        || !app
            .current
            .output_selection
            .is_some_and(|selection| selection.dragging)
    {
        return;
    }
    let _ = app
        .current
        .scroll_messages(if direction < 0 { 1 } else { -1 });
    let Some(layout) = &app.current.message_layout else {
        return;
    };
    let scroll = app.current.output_scroll_top.unwrap_or(layout.scroll);
    let row = if direction < 0 {
        scroll
    } else {
        scroll.saturating_add(layout.viewport.height.saturating_sub(1) as usize)
    };
    let column = relative_output_column(app.current.edge_scroll.column, layout.viewport);
    if let Some(offset) = layout.position_at_visual_row(row, column)
        && let Some(selection) = &mut app.current.output_selection
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
}

fn cancel_active_request(app: &mut App) {
    if let Some(approval) = app.current.take_pending_approval() {
        app.force_full_redraw = true;
        if let ApprovalAction::Agent(reply) = approval.action {
            let _ = reply.send(false);
        }
    }
    if let Some(task) = app.current.active_task.take() {
        task.abort();
    }
    app.current.finish_thinking("思考已取消");
    app.current.busy = false;
    app.current.agent_phase = AgentPhase::Idle;
    app.current.model_phase = ModelPhase::Idle;
    app.current.status = "已取消当前请求".into();
    app.current.push_entry(DisplayEntry {
        kind: DisplayKind::System,
        content: DisplayContent::Markdown("当前请求已取消。".into()),
    });
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
        app.current.push_entry(DisplayEntry {
            kind: DisplayKind::Error,
            content: DisplayContent::Markdown(format!("未知命令，请使用 /help 查看命令：{input}")),
        });
        return Ok(());
    }
    let Some(runner) = app.current.runner.clone() else {
        app.current.status = "请打开提供商设置配置 API Key".into();
        return Ok(());
    };
    app.input.clear();
    app.file_suggestions.clear();
    app.current.clear_output_selection();
    app.current.message_scroll = 0;
    app.current.follow_output = true;
    app.current.output_scroll_top = None;
    app.current.push_entry(DisplayEntry {
        kind: DisplayKind::User,
        content: DisplayContent::Markdown(input.clone()),
    });
    app.current.conversation.push(ConversationItem::Message {
        role: Role::User,
        content: input.clone(),
    });
    app.storage
        .append_message(&app.current.session_id, Role::User, &input)?;
    for (label, content) in collect_file_context(app, &input) {
        app.current.conversation.push(ConversationItem::Context {
            label: label.clone(),
            content: content.clone(),
        });
        app.storage
            .append_context(&app.current.session_id, &label, &content)?;
        app.current.push_entry(DisplayEntry {
            kind: DisplayKind::System,
            content: DisplayContent::Markdown(format!("已附加文件 @{label}")),
        });
    }
    refresh_sessions(app)?;
    trim_conversation(&mut app.current.conversation);
    app.current.context_used_tokens = estimate_context_tokens(&app.current.conversation);
    app.current.busy = true;
    app.current.agent_phase = AgentPhase::Thinking;
    app.current.model_phase = ModelPhase::Idle;
    app.current.status = "准备请求中…… | Esc 取消".into();
    let items = app.current.conversation.clone();
    let events = app.current.agent_tx.clone();
    app.current.active_task = Some(tokio::spawn(async move {
        runner.run(items, events).await;
    }));
    app.current.trim_entries();
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
    app.settings = Some(SettingsState::list(
        app.config.providers.clone(),
        app.config.provider.preset,
    ));
    app.current.status = "已连接的供应商".into();
}

fn available_key_presets() -> HashSet<ProviderPreset> {
    ProviderPreset::ALL
        .iter()
        .filter_map(|preset| secrets::api_key_cached_only(*preset).ok().map(|_| *preset))
        .collect()
}

fn provider_form(app: &App, provider: crate::config::ProviderConfig) -> SettingsForm {
    let existing_key_preset = app.active_secret.as_ref().map(|(preset, _)| *preset);
    let mut form = SettingsForm::new(provider, existing_key_preset);
    form.set_available_key_presets(available_key_presets());
    form
}

fn reopen_provider_list(app: &mut App) {
    app.settings = Some(SettingsState::list(
        app.config.providers.clone(),
        app.config.provider.preset,
    ));
}

fn open_provider_form(app: &mut App, provider: crate::config::ProviderConfig) {
    app.settings = Some(SettingsState::Form(provider_form(app, provider)));
}

fn open_template_picker(app: &mut App) {
    if let Some(settings) = &mut app.settings {
        settings.open_templates();
        app.current.status = "选择供应商模板".into();
    }
}

fn open_selected_profile(app: &mut App) {
    if let Some(provider) = app
        .settings
        .as_ref()
        .and_then(SettingsState::selected_profile)
    {
        let _ = secrets::api_key_cached(provider.preset);
        open_provider_form(app, provider);
        app.current.status = "编辑供应商".into();
    }
}

fn open_selected_template(app: &mut App) {
    if let Some(preset) = app
        .settings
        .as_ref()
        .and_then(SettingsState::selected_template)
    {
        open_provider_form(app, preset.defaults());
        app.current.status = format!("添加 {}", preset.label());
    }
}

fn remove_settings_provider(app: &mut App) -> Result<()> {
    let preset = app
        .settings
        .as_ref()
        .and_then(SettingsState::form)
        .map(|form| form.provider.preset)
        .context("provider editor is not open")?;
    app.config.remove_provider(preset);
    if app.config.provider.preset == preset {
        app.config.provider = app
            .config
            .providers
            .first()
            .cloned()
            .unwrap_or_else(|| ProviderPreset::OpenAi.defaults());
        app.active_secret = secrets::api_key_cached(app.config.provider.preset)
            .ok()
            .map(|key| (app.config.provider.preset, key));
        rebuild_runner(app)?;
    }
    app.config.save()?;
    reopen_provider_list(app);
    app.current.status = "供应商已移除；API Key 已保留在系统钥匙串".into();
    Ok(())
}

fn handle_palette_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Some(palette) = &mut app.palette else {
        return;
    };
    let results = commands::matches(&palette.query, 10);
    match code {
        KeyCode::Esc => {
            app.palette = None;
            app.current.status = "就绪".into();
        }
        KeyCode::Enter => {
            let selected = results.get(palette.selected).copied();
            let action = selected.map(|item| commands::PALETTE_ITEMS[item.index].action);
            app.palette = None;
            if let Some(action) = action {
                if let Err(error) = execute_palette_action(app, action) {
                    app.current.status = format!("命令失败：{error}");
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

fn open_palette(app: &mut App) {
    app.palette = Some(CommandPaletteState {
        query: String::new(),
        selected: 0,
    });
    app.current.status = "命令面板 | 输入筛选 | ↑/↓ 选择 | Enter 执行 | Esc 关闭".into();
}

fn execute_palette_action(app: &mut App, action: commands::PaletteAction) -> Result<()> {
    match action {
        commands::PaletteAction::Command(input) => {
            let command = commands::parse(input).context("invalid palette command")?;
            execute_command(app, command)
        }
        commands::PaletteAction::CycleMode => switch_mode(app, next_mode(app.current.mode)),
    }
}

fn execute_command(app: &mut App, command: Command) -> Result<()> {
    match command {
        Command::Help => {
            app.current.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(
                    "## 命令\n\n`/new` `/rename` `/fork` `/delete`\n`/undo` `/redo` `/compact` `/export [路径]` `/todo [add|doing|done|undo|edit|remove|clear]` `/diff`\n`/plan` `/build` `/explore` `/model` `/provider`\n\nCtrl+P 或 Ctrl+X 打开命令面板 | @ 文件 | ! Shell"
                        .into(),
                ),
            });
            app.current.status = "命令帮助".into();
        }
        Command::NewSession => create_session(app)?,
        Command::Provider => open_settings(app),
        Command::Model(model) => {
            if let Some(model) = model {
                if model.trim().is_empty() {
                    app.current.status = "模型不能为空".into();
                } else {
                    apply_model_choice(app, model.trim().to_owned())?;
                }
            } else {
                app.current.status = format!("当前模型：{}", app.config.provider.model);
            }
        }
        Command::Agent(agent) => {
            if let Some(name) = agent {
                if let Some(configured) = app.config.agents.iter().find(|item| item.name == name) {
                    app.current.mode = configured.mode;
                    app.registry.set_mode(app.current.mode);
                    app.storage
                        .set_session_mode(&app.current.session_id, app.current.mode.as_str())?;
                    // Force a fresh provider context so the new mode contract is
                    // sent as the stable system prefix on the next request.
                    app.storage.clear_response_id(&app.current.session_id)?;
                    app.current.status = format!("Agent：{} | 模式：{}", name, app.current.mode);
                    app.current.push_entry(DisplayEntry {
                        kind: DisplayKind::System,
                        content: DisplayContent::Markdown(format!(
                            "Agent 模式已切换为 **{}**。下一次模型请求将使用 {} 执行约束。",
                            app.current.mode.as_str().to_ascii_uppercase(),
                            app.current.mode.as_str()
                        )),
                    });
                } else {
                    app.current.status = format!("未知 Agent：{name}");
                }
            } else {
                app.current.status = format!("当前 Agent 模式：{}", app.current.mode);
            }
        }
        Command::Mode(mode) => {
            switch_mode(app, mode)?;
            app.current.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(format!(
                    "Agent 模式已切换为 **{}**。下一次模型请求将使用 {} 执行约束。",
                    mode.as_str().to_ascii_uppercase(),
                    mode.as_str()
                )),
            });
        }
        Command::Clear => {
            app.current.invalidate_output_layout();
            app.current.entries.clear();
            app.current.reset_thinking_state();
            app.current.clear_output_selection();
            app.current.status = "显示已清空，会话历史仍保留".into();
        }
        Command::Quit => app.should_quit = true,
        Command::Rename(title) => {
            let Some(title) = title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
            else {
                app.input.set("/rename ");
                app.current.status = "请输入新会话名称：/rename <名称>".into();
                return Ok(());
            };
            app.storage.rename_session(&app.current.session_id, title)?;
            refresh_sessions(app)?;
            app.current.status = format!("会话已重命名为 {title}");
        }
        Command::Delete => {
            let deleted = app.current.session_id.clone();
            let deleted_ids = app.storage.delete_session(&deleted)?;
            let next = match app.storage.latest_session(&app.workspace)? {
                Some(session_id) => session_id,
                None => app.storage.create_session(&app.workspace)?,
            };
            activate_session(app, next)?;
            let deleted_ids = deleted_ids.into_iter().collect::<HashSet<_>>();
            for session_id in &deleted_ids {
                if let Some(mut runtime) = app.background.remove(session_id) {
                    runtime.shutdown();
                }
                app.child_status.remove(session_id);
                app.child_batches.remove(session_id);
                app.expanded_sessions.remove(session_id);
            }
            app.child_batches.retain(|_, children| {
                children.retain(|child_id| !deleted_ids.contains(child_id));
                !children.is_empty()
            });
            refresh_sessions(app)?;
            app.current.status = "会话已删除".into();
        }
        Command::Fork => {
            let fork = app.storage.fork_session(&app.current.session_id)?;
            activate_session(app, fork)?;
            refresh_sessions(app)?;
            app.current.status = "会话已创建分支".into();
        }
        Command::Undo => {
            if app.storage.undo(&app.current.session_id)? {
                app.storage.clear_response_id(&app.current.session_id)?;
                reload_current_session(app)?;
                refresh_sessions(app)?;
                app.current.status = "已撤销上一轮".into();
            } else {
                app.current.status = "没有可撤销的内容".into();
            }
        }
        Command::Redo => {
            if app.storage.redo(&app.current.session_id)? {
                app.storage.clear_response_id(&app.current.session_id)?;
                reload_current_session(app)?;
                refresh_sessions(app)?;
                app.current.status = "已重做上一轮".into();
            } else {
                app.current.status = "没有可重做的内容".into();
            }
        }
        Command::Todo(action) => handle_todo_command(app, action)?,
        Command::Compact(focus) => {
            let Some(runner) = app.current.runner.clone() else {
                app.current.status = "请打开提供商设置配置 API Key".into();
                return Ok(());
            };
            let mut items = app.current.conversation.clone();
            let events = app.current.agent_tx.clone();
            let focus = focus.map(|value| value.trim().to_owned());
            app.current.busy = true;
            app.current.status = "准备压缩上下文…… | Esc 取消".into();
            app.current.active_task = Some(tokio::spawn(async move {
                match runner
                    .compact_context(&mut items, focus.as_deref(), &events)
                    .await
                {
                    Ok(_) => {
                        let _ = events.send(AgentEvent::Completed { items }).await;
                    }
                    Err(error) => {
                        trim_conversation(&mut items);
                        let _ = events.send(AgentEvent::CompactionFailed(error)).await;
                        let _ = events.send(AgentEvent::Completed { items }).await;
                    }
                }
            }));
        }
        Command::Uncompact => {
            if app
                .storage
                .restore_latest_compaction(&app.current.session_id)?
            {
                let session_id = app.current.session_id.clone();
                activate_session(app, session_id)?;
                app.current.status = "已恢复最近一次压缩".into();
            } else {
                app.current.status = "没有可恢复的压缩检查点".into();
            }
        }
        Command::Export(path) => export_session(app, path)?,
        Command::Diff => start_diff(app)?,
    }
    Ok(())
}

fn handle_todo_command(app: &mut App, action: TodoCommand) -> Result<()> {
    match action {
        TodoCommand::Show => {
            let (done, total) = todo_progress(&app.current.todos);
            let mut content = format!("## 任务清单 {done}/{total}\n");
            for (index, task) in app.current.todos.iter().enumerate() {
                content.push_str(&format!(
                    "- {} {}. {}\n",
                    task.status.symbol(),
                    index + 1,
                    task.title
                ));
            }
            app.current.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(content),
            });
            app.current.status = if total == 0 {
                "任务清单为空".into()
            } else {
                format!("任务清单 {done}/{total}")
            };
        }
        TodoCommand::Add(title) => {
            let mut tasks = app.current.todos.clone();
            tasks.push(TodoTask::new(title, TodoStatus::Pending));
            apply_todo_tasks(app, tasks)?;
            app.current.status = "任务已添加".into();
        }
        TodoCommand::Doing(index) => {
            update_todo_status(app, index, TodoStatus::InProgress, "任务已标记为进行中")?;
        }
        TodoCommand::Done(index) => {
            update_todo_status(app, index, TodoStatus::Done, "任务已完成")?;
        }
        TodoCommand::Undo(index) => {
            update_todo_status(app, index, TodoStatus::Pending, "任务已标记为待处理")?;
        }
        TodoCommand::Edit(index, title) => {
            let mut tasks = app.current.todos.clone();
            let Some(task) = tasks.get_mut(index.checked_sub(1).unwrap_or(usize::MAX)) else {
                app.current.status = "任务序号不存在".into();
                return Ok(());
            };
            task.title = title;
            task.updated_at = chrono::Utc::now().to_rfc3339();
            apply_todo_tasks(app, tasks)?;
            app.current.status = "任务已更新".into();
        }
        TodoCommand::Remove(index) => {
            let mut tasks = app.current.todos.clone();
            if index == 0 || index > tasks.len() {
                app.current.status = "任务序号不存在".into();
                return Ok(());
            }
            tasks.remove(index - 1);
            apply_todo_tasks(app, tasks)?;
            app.current.status = "任务已删除".into();
        }
        TodoCommand::Clear => {
            apply_todo_tasks(app, Vec::new())?;
            app.current.status = "任务清单已清空".into();
        }
    }
    Ok(())
}

fn todo_progress(tasks: &[TodoTask]) -> (usize, usize) {
    (
        tasks
            .iter()
            .filter(|task| task.status == TodoStatus::Done)
            .count(),
        tasks.len(),
    )
}

fn update_todo_status(
    app: &mut App,
    index: usize,
    status: TodoStatus,
    message: &str,
) -> Result<()> {
    let mut tasks = app.current.todos.clone();
    let Some(task) = tasks.get_mut(index.checked_sub(1).unwrap_or(usize::MAX)) else {
        app.current.status = "任务序号不存在".into();
        return Ok(());
    };
    task.status = status;
    task.updated_at = chrono::Utc::now().to_rfc3339();
    apply_todo_tasks(app, tasks)?;
    app.current.status = message.into();
    Ok(())
}

fn apply_todo_tasks(app: &mut App, tasks: Vec<TodoTask>) -> Result<()> {
    app.storage.replace_tasks(&app.current.session_id, &tasks)?;
    app.current.todos = tasks;
    app.current.invalidate_output_layout();
    Ok(())
}

fn export_session(app: &mut App, requested: Option<String>) -> Result<()> {
    let requested = requested
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let default_filename = format!("1h-agent-{}.md", app.current.session_id);
    let filename = requested.unwrap_or(default_filename.as_str());
    let target = app
        .registry
        .workspace()
        .resolve_new(filename)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let mut output = String::new();
    if !app.current.todos.is_empty() {
        let (done, total) = todo_progress(&app.current.todos);
        output.push_str(&format!("## 任务清单（{done}/{total}）\n\n"));
        for task in &app.current.todos {
            let checkbox = if task.status == TodoStatus::Done {
                "x"
            } else {
                " "
            };
            let suffix = if task.status == TodoStatus::InProgress {
                "（进行中）"
            } else {
                ""
            };
            output.push_str(&format!("- [{checkbox}] {}{suffix}\n", task.title));
        }
        output.push('\n');
    }
    for item in &app.current.conversation {
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
    let display_path = match target.strip_prefix(&app.workspace) {
        Ok(path) => path.display(),
        Err(_) => target.display(),
    };
    app.current.status = format!("对话已导出到工作区 {}", display_path);
    Ok(())
}

fn rebuild_runner(app: &mut App) -> Result<()> {
    let Some((_, api_key)) = &app.active_secret else {
        app.current.runner = None;
        return Ok(());
    };
    let provider = OpenAiClient::new(app.config.provider.base_url.clone(), api_key.clone())?;
    let child_role = app.current.child_role.clone();
    let child_provider_resolver = provider_config_resolver(&app.config);
    app.current.runner = Some(
        AgentRunner::new(
            provider,
            app.config.provider.clone(),
            app.registry.clone(),
            app.storage.clone(),
            app.current.session_id.clone(),
        )
        .with_cluster_config(app.config.cluster.clone())
        .with_approval_lock(app.approval_lock.clone())
        .with_configured_agents(app.config.agents.clone())
        .with_child_role(child_role)
        .with_child_provider_resolver(child_provider_resolver),
    );
    Ok(())
}

fn start_diff(app: &mut App) -> Result<()> {
    if app.current.busy {
        return Ok(());
    }
    let registry = app.registry.clone();
    let events = app.current.agent_tx.clone();
    app.current.busy = true;
    app.current.status = "正在收集 Git diff…… | Esc 取消".into();
    app.current.active_task = Some(tokio::spawn(async move {
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

fn paste_text_into_settings(app: &mut App, text: &str) -> bool {
    let Some(form) = app.settings.as_mut().and_then(SettingsState::form_mut) else {
        return false;
    };
    let field = form.field();
    if !matches!(
        field,
        SettingsField::Model | SettingsField::BaseUrl | SettingsField::ApiKey
    ) {
        return false;
    }
    let sanitized = text.replace(['\r', '\n'], "");
    let mut sanitized = sanitized.as_str();
    if sanitized.len() > crate::clipboard::MAX_CLIPBOARD_BYTES {
        let mut end = crate::clipboard::MAX_CLIPBOARD_BYTES;
        while end > 0 && !sanitized.is_char_boundary(end) {
            end -= 1;
        }
        sanitized = &sanitized[..end];
    }
    form.paste(field, sanitized);
    true
}

fn handle_settings_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            if matches!(app.settings, Some(SettingsState::List(_))) {
                app.settings = None;
                app.current.status = "设置已取消".into();
            } else {
                reopen_provider_list(app);
                app.current.status = "已返回供应商列表".into();
            }
        }
        KeyCode::Tab | KeyCode::Down => {
            if let Some(settings) = &mut app.settings {
                settings.move_selection(1);
            }
        }
        KeyCode::BackTab | KeyCode::Up => {
            if let Some(settings) = &mut app.settings {
                settings.move_selection(-1);
            }
        }
        KeyCode::Left | KeyCode::Right => {
            let direction = if code == KeyCode::Right { 1 } else { -1 };
            if let Some(form) = app.settings.as_mut().and_then(SettingsState::form_mut) {
                let field = form.field();
                form.cycle(field, direction);
            }
        }
        KeyCode::Backspace => {
            if let Some(form) = app.settings.as_mut().and_then(SettingsState::form_mut) {
                let field = form.field();
                form.edit(field, None);
            }
        }
        KeyCode::Char('v')
            if modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::CONTROL | KeyModifiers::META) =>
        {
            match crate::clipboard::read_text() {
                Ok(text) => {
                    if paste_text_into_settings(app, &text) {
                        app.current.status = "已粘贴剪贴板内容".into();
                    } else {
                        app.current.status = "当前字段不支持粘贴".into();
                    }
                }
                Err(error) => {
                    app.current.status = format!("无法读取系统剪贴板：{}", secrets::redact(&error));
                }
            }
        }
        KeyCode::Delete | KeyCode::Char('d')
            if matches!(app.settings, Some(SettingsState::Form(_)))
                && (code == KeyCode::Delete || modifiers.contains(KeyModifiers::CONTROL)) =>
        {
            if let Err(error) = remove_settings_provider(app) {
                app.current.status = format!("移除失败：{}", secrets::redact(&error.to_string()));
            }
        }
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(form) = app.settings.as_mut().and_then(SettingsState::form_mut) {
                let field = form.field();
                form.edit(field, Some(character));
            }
        }
        KeyCode::Enter => match app.settings.as_ref() {
            Some(settings) if settings.on_add_row() => open_template_picker(app),
            Some(SettingsState::List(_)) => open_selected_profile(app),
            Some(SettingsState::Templates(_)) => open_selected_template(app),
            Some(SettingsState::Form(_)) => {
                if let Err(error) = apply_settings(app) {
                    app.current.status =
                        format!("设置错误：{}", secrets::redact(&error.to_string()));
                }
            }
            None => {}
        },
        _ => {}
    }
}

fn apply_settings(app: &mut App) -> Result<()> {
    let (provider_config, api_key, entered_key) = {
        let form = app
            .settings
            .as_ref()
            .and_then(SettingsState::form)
            .context("provider editor is not open")?;
        (
            form.prepare()?,
            form.resolve_api_key(app.active_secret.as_ref())?,
            form.api_key.trim().to_owned(),
        )
    };

    app.active_secret = Some((provider_config.preset, api_key));
    app.config.provider = provider_config.clone();
    app.config.upsert_provider(provider_config.clone());
    app.current.context_limit_tokens = provider_config.resolved_context_window_tokens();
    rebuild_runner(app)?;

    let key_warning = if !entered_key.is_empty() {
        secrets::store_api_key_cached(provider_config.preset, &entered_key)
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
    reopen_provider_list(app);
    app.current.status = format!(
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

/// Routes an agent event to its owning session. Events for the active session
/// are applied directly and trigger a redraw; events for a background session
/// are applied to that session's runtime in place, without redrawing or
/// disturbing the active session's status.
fn handle_routed_event(app: &mut App, routed: RoutedEvent) -> bool {
    let RoutedEvent { session_id, event } = routed;
    let is_active = session_id == app.active_session;
    if let AgentEvent::ChildSessionProgress {
        session_id: child_id,
        progress,
    } = &event
    {
        let previous_batch_finished = app.child_batches.get(&session_id).is_some_and(|children| {
            !children.is_empty()
                && children.iter().all(|child| {
                    app.child_status
                        .get(child)
                        .is_some_and(|progress| progress.status.is_terminal())
                })
        });
        if progress.status == ChildSessionStatus::Queued && previous_batch_finished {
            app.child_batches.remove(&session_id);
        }
        app.child_batches
            .entry(session_id.clone())
            .or_default()
            .insert(child_id.clone());
        app.child_status.insert(child_id.clone(), progress.clone());
        let _ = refresh_sessions(app);
        update_cluster_batch_status(app, &session_id);
        return true;
    }
    let outcome = {
        let ctx = EventCtx {
            storage: &app.storage,
            workspace: &app.workspace,
        };
        if is_active {
            app.current.handle_event(&ctx, event)
        } else if let Some(rt) = app.background.get_mut(&session_id) {
            rt.handle_event(&ctx, event)
        } else {
            return false;
        }
    };
    if outcome.force_redraw && is_active {
        app.force_full_redraw = true;
    }
    if outcome.sessions_dirty && refresh_sessions(app).is_err() && is_active {
        app.current.status = "就绪，但刷新会话失败".into();
    }
    if !is_active {
        evict_background_overflow(app);
    }
    is_active || app.has_pending_approval()
}

fn update_cluster_batch_status(app: &mut App, parent_id: &str) {
    let Some(children) = app.child_batches.get(parent_id) else {
        return;
    };
    let total = children.len();
    let completed = children
        .iter()
        .filter(|child| {
            app.child_status
                .get(*child)
                .is_some_and(|progress| progress.status.is_terminal())
        })
        .count();
    let queued = children
        .iter()
        .filter(|child| {
            app.child_status
                .get(*child)
                .is_some_and(|progress| progress.status == ChildSessionStatus::Queued)
        })
        .count();
    let running = total.saturating_sub(completed + queued);
    let status = format!("集群 {completed}/{total} 完成 · {running} 运行 · {queued} 排队");
    if let Some(runtime) = app.runtime_mut(parent_id) {
        runtime.status = status;
    }
}

fn should_coalesce_stream_redraw(active_session: &str, routed: &RoutedEvent) -> bool {
    routed.session_id == active_session
        && matches!(
            &routed.event,
            AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta(_)
        )
}

fn should_coalesce_terminal_redraw(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse)
            if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
    )
}

fn schedule_deferred_redraw(timer: &mut Option<Pin<Box<tokio::time::Sleep>>>) {
    if timer.is_none() {
        *timer = Some(Box::pin(tokio::time::sleep(DEFERRED_REDRAW_INTERVAL)));
    }
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

fn resolve_approval(app: &mut App, approved: bool) {
    if let Some((owner, approval)) = app.take_pending_approval_global() {
        app.force_full_redraw = true;
        match approval.action {
            ApprovalAction::Agent(reply) => {
                let _ = reply.send(approved);
                if let Some(runtime) = app.runtime_mut(&owner) {
                    runtime.agent_phase = AgentPhase::Thinking;
                    runtime.model_phase = ModelPhase::Idle;
                    runtime.status = if approved {
                        "已批准，开始执行工具……".into()
                    } else {
                        "已拒绝，将结果返回模型……".into()
                    };
                }
            }
            ApprovalAction::Shell(command) => {
                if approved {
                    let registry = app.registry.clone();
                    let Some(runtime) = app.runtime_mut(&owner) else {
                        return;
                    };
                    let events = runtime.agent_tx.clone();
                    let command_for_event = command.clone();
                    runtime.busy = true;
                    runtime.agent_phase = AgentPhase::ToolRunning;
                    runtime.model_phase = ModelPhase::Idle;
                    runtime.status = "正在执行 Shell 命令…… | Esc 取消".into();
                    runtime.active_task = Some(tokio::spawn(async move {
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
                } else if let Some(runtime) = app.runtime_mut(&owner) {
                    runtime.push_entry(DisplayEntry {
                        kind: DisplayKind::System,
                        content: DisplayContent::Markdown("Shell 命令已拒绝。".into()),
                    });
                    runtime.status = "Shell 命令已拒绝".into();
                    runtime.agent_phase = AgentPhase::Idle;
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
    app.current.pending_approval = Some(PendingApproval {
        call,
        reason: "! 命令将通过 workspace Shell 执行".into(),
        source_session_id: None,
        source_title: None,
        action: ApprovalAction::Shell(command),
        created_at: Instant::now(),
    });
    app.current.agent_phase = AgentPhase::WaitingApproval;
    app.current.model_phase = ModelPhase::Idle;
    app.current.status = "Shell 命令需要确认".into();
    Ok(())
}

fn create_session(app: &mut App) -> Result<()> {
    let session_id = app.storage.create_session(&app.workspace)?;
    activate_session(app, session_id)?;
    refresh_sessions(app)?;
    app.current.status = "新会话已就绪".into();
    Ok(())
}

fn next_mode(mode: AgentMode) -> AgentMode {
    match mode {
        AgentMode::Build => AgentMode::Plan,
        AgentMode::Plan => AgentMode::Explore,
        AgentMode::Explore => AgentMode::Cluster,
        AgentMode::Cluster => AgentMode::Build,
    }
}

/// Shared mode-switch entry point for slash commands, palette actions, and
/// clicking the mode label in the input title. It updates UI state,
/// tool permissions, persistence, and clears the provider response id so the
/// next request uses the new mode contract.
fn switch_mode(app: &mut App, mode: AgentMode) -> Result<()> {
    app.current.mode = mode;
    app.registry.set_mode(mode);
    let _ = app
        .storage
        .set_session_mode(&app.current.session_id, mode.as_str());
    app.storage.clear_response_id(&app.current.session_id)?;
    app.current.status = format!("模式已切换为 {}", mode.as_str().to_ascii_uppercase());
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
    let rows = ui::flatten_session_tree(&app.sessions, &app.expanded_sessions);
    if rows.len() < 2 {
        app.current.status = "当前只有一个会话 | Ctrl+N 新建会话".into();
        return Ok(());
    }
    let current = rows
        .iter()
        .position(|row| row.id == app.current.session_id)
        .unwrap_or(0) as i32;
    let next = (current + direction).rem_euclid(rows.len() as i32) as usize;
    let session_id = rows[next].id.clone();
    activate_session(app, session_id)
}

fn provider_config_resolver(config: &Config) -> Arc<crate::agent::ChildProviderResolver> {
    let providers = config.providers.clone();
    let default_provider = config.provider.clone();
    Arc::new(
        move |preset: ProviderPreset| -> Result<crate::config::ProviderConfig, String> {
            if let Some(provider) = providers
                .iter()
                .find(|provider| provider.preset == preset)
                .cloned()
            {
                return Ok(provider);
            }
            if preset == default_provider.preset {
                return Ok(default_provider.clone());
            }
            let mut provider_config = preset.defaults();
            provider_config
                .validate()
                .map_err(|error| format!("invalid child provider configuration: {error}"))?;
            Ok(provider_config)
        },
    )
}

/// Resolves a stored provider id/model pair for a session. Child sessions may
/// reference a different provider than the current global setting; in that case
/// the preset defaults are used (and must be valid, e.g. Qwen needs a real
/// workspace URL configured via env or config).
fn session_provider_config(
    config: &Config,
    provider_id: &str,
    model: &str,
) -> Option<crate::config::ProviderConfig> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let preset = ProviderPreset::parse(provider_id)?;
    let mut provider_config = config
        .provider_for(preset)
        .unwrap_or_else(|| preset.defaults());
    provider_config.validate().ok()?;
    provider_config.model = model.to_owned();
    provider_config.normalize_thinking();
    Some(provider_config)
}

/// Builds a fresh `SessionRuntime` for the given session: loads its messages,
/// resolves provider/model (child sessions override the global default), and
/// spawns an event forwarder that routes its agent events to the router.
fn build_runtime(
    storage: &Storage,
    config: &Config,
    registry: &Arc<ToolRegistry>,
    router_tx: &mpsc::Sender<RoutedEvent>,
    approval_lock: &Arc<Mutex<()>>,
    active_secret: Option<&(ProviderPreset, String)>,
    session_id: &str,
) -> SessionRuntime {
    let mut conversation = storage.load_messages(session_id).unwrap_or_default();
    trim_conversation(&mut conversation);
    let todos = storage.list_tasks(session_id).unwrap_or_default();
    let entries = display_entries(&conversation);
    let mode = storage
        .session_mode(session_id)
        .ok()
        .and_then(|value| AgentMode::parse(&value))
        .unwrap_or_default();
    let provider_config = storage
        .session_provider_model(session_id)
        .ok()
        .and_then(|(provider_id, model)| session_provider_config(config, &provider_id, &model))
        .unwrap_or_else(|| config.provider.clone());
    let child_role = storage.session_child_role(session_id).ok().flatten();
    let child_provider_resolver = provider_config_resolver(config);
    let runtime_key = active_secret
        .filter(|(preset, _)| *preset == provider_config.preset)
        .map(|(_, api_key)| api_key.clone())
        .or_else(|| secrets::api_key_cached_only(provider_config.preset).ok());
    let runner = runtime_key.as_ref().and_then(|api_key| {
        OpenAiClient::new(provider_config.base_url.clone(), api_key.clone())
            .ok()
            .map(|provider| {
                AgentRunner::new(
                    provider,
                    provider_config.clone(),
                    registry.clone(),
                    storage.clone(),
                    session_id.to_owned(),
                )
                .with_cluster_config(config.cluster.clone())
                .with_approval_lock(approval_lock.clone())
                .with_configured_agents(config.agents.clone())
                .with_compaction_config(config.compaction.clone())
                .with_child_role(child_role.clone())
                .with_child_provider_resolver(child_provider_resolver)
            })
    });
    let (agent_tx, agent_rx) = mpsc::channel(128);
    let router = router_tx.clone();
    let sid = session_id.to_owned();
    tokio::spawn(async move {
        let mut receiver = agent_rx;
        while let Some(event) = receiver.recv().await {
            if router
                .send(RoutedEvent {
                    session_id: sid.clone(),
                    event,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    SessionRuntime {
        session_id: session_id.to_owned(),
        status: String::new(),
        entries,
        todos,
        busy: false,
        agent_phase: AgentPhase::Idle,
        model_phase: ModelPhase::Idle,
        thinking_last_line: String::new(),
        thinking_active: false,
        thinking_buffer: String::new(),
        thinking_buffer_truncated: false,
        thinking_buffer_epoch: 0,
        live_thinking_layout_cache: Default::default(),
        thinking_animation_frame: 0,
        thinking_anchor: None,
        thinking_result: ThinkingResult::Completed,
        usage: Usage::default(),
        context_used_tokens: estimate_context_tokens(&conversation),
        context_limit_tokens: provider_config.resolved_context_window_tokens(),
        pending_approval: None,
        mode,
        child_role,
        expanded_tools: HashSet::new(),
        expanded_thinking: HashSet::new(),
        thinking_expanded: false,
        message_scroll: 0,
        follow_output: true,
        output_scroll_top: None,
        output_selection: None,
        message_layout: None,
        markdown_render_cache: HashMap::new(),
        output_layout_dirty: true,
        #[cfg(test)]
        output_layout_rebuild_count: 0,
        #[cfg(test)]
        markdown_parse_count: 0,
        #[cfg(test)]
        footer_rebuild_count: 0,
        edge_scroll: EdgeScroll::default(),
        conversation,
        runner,
        agent_tx,
        active_task: None,
        parked_at: Instant::now(),
    }
}

fn reload_current_session(app: &mut App) -> Result<()> {
    let session_id = app.active_session.clone();
    let active_secret = app.active_secret.clone();
    let target = build_runtime(
        &app.storage,
        &app.config,
        &app.registry,
        &app.router_tx,
        &app.approval_lock,
        active_secret.as_ref(),
        &session_id,
    );
    let mut old = std::mem::replace(&mut app.current, target);
    old.shutdown();
    app.registry.set_mode(app.current.mode);
    app.input.clear();
    app.file_suggestions.clear();
    app.current.invalidate_output_layout();
    Ok(())
}

fn activate_session(app: &mut App, session_id: String) -> Result<()> {
    if session_id == app.active_session {
        return Ok(());
    }
    // Pull the target runtime from the background (preserving any in-flight
    // agent state) or build it fresh; the current runtime is parked so its
    // agent keeps running in the background.
    let active_secret = app.active_secret.clone();
    let target = app.background.remove(&session_id).unwrap_or_else(|| {
        build_runtime(
            &app.storage,
            &app.config,
            &app.registry,
            &app.router_tx,
            &app.approval_lock,
            active_secret.as_ref(),
            &session_id,
        )
    });
    let old_id = app.active_session.clone();
    let mut old = std::mem::replace(&mut app.current, target);
    old.parked_at = Instant::now();
    app.background.insert(old_id, old);
    evict_background_overflow(app);
    app.active_session = session_id;
    app.registry.set_mode(app.current.mode);
    app.input.clear();
    app.file_suggestions.clear();
    app.current.invalidate_output_layout();
    app.current.status = if app.current.runner.is_some() {
        "就绪".into()
    } else {
        "需要配置提供商".into()
    };
    Ok(())
}

fn evict_background_overflow(app: &mut App) {
    let capacity = app.config.runtime.max_background_sessions;
    while app.background.len() > capacity {
        let eviction_id = app
            .background
            .iter()
            .filter(|(_, runtime)| runtime.idle())
            .min_by_key(|(_, runtime)| runtime.parked_at)
            .or_else(|| {
                app.background
                    .iter()
                    .min_by_key(|(_, runtime)| runtime.parked_at)
            })
            .map(|(session_id, _)| session_id.clone());
        let Some(eviction_id) = eviction_id else {
            break;
        };
        if let Some(mut runtime) = app.background.remove(&eviction_id) {
            runtime.shutdown();
        }
    }
}

fn refresh_sessions(app: &mut App) -> Result<()> {
    app.sessions = app.storage.list_sessions(&app.workspace)?;
    let live_ids = app
        .sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<HashSet<_>>();
    app.child_status
        .retain(|session_id, _| live_ids.contains(session_id.as_str()));
    app.child_batches.retain(|session_id, children| {
        if !live_ids.contains(session_id.as_str()) {
            return false;
        }
        children.retain(|child_id| live_ids.contains(child_id.as_str()));
        !children.is_empty()
    });
    app.expanded_sessions
        .retain(|session_id| live_ids.contains(session_id.as_str()));
    Ok(())
}

impl App {
    pub(crate) fn has_pending_approval(&self) -> bool {
        self.pending_approval().is_some()
    }

    pub(crate) fn pending_approval(&self) -> Option<&PendingApproval> {
        let current = self
            .current
            .pending_approval
            .as_ref()
            .map(|approval| (approval.created_at, approval));
        self.background
            .values()
            .filter_map(|runtime| {
                runtime
                    .pending_approval
                    .as_ref()
                    .map(|approval| (approval.created_at, approval))
            })
            .chain(current)
            .min_by_key(|(created_at, _)| *created_at)
            .map(|(_, approval)| approval)
    }

    fn take_pending_approval_global(&mut self) -> Option<(String, PendingApproval)> {
        let mut owner = self
            .current
            .pending_approval
            .as_ref()
            .map(|approval| (approval.created_at, self.active_session.clone()));
        for (session_id, runtime) in &self.background {
            if let Some(approval) = &runtime.pending_approval
                && owner
                    .as_ref()
                    .is_none_or(|(created_at, _)| approval.created_at < *created_at)
            {
                owner = Some((approval.created_at, session_id.clone()));
            }
        }
        let (_, owner) = owner?;
        let approval = if owner == self.active_session {
            self.current.take_pending_approval()
        } else {
            self.background
                .get_mut(&owner)
                .and_then(SessionRuntime::take_pending_approval)
        }?;
        Some((owner, approval))
    }

    fn runtime_mut(&mut self, session_id: &str) -> Option<&mut SessionRuntime> {
        if session_id == self.active_session {
            Some(&mut self.current)
        } else {
            self.background.get_mut(session_id)
        }
    }

    /// Whether a session currently has an approval prompt waiting. This is used
    /// by the session panel so background parent sessions are not stuck waiting
    /// for an invisible approval.
    pub(crate) fn session_waiting_approval(&self, session_id: &str) -> bool {
        if session_id == self.active_session {
            self.current.pending_approval.is_some()
        } else {
            self.background
                .get(session_id)
                .is_some_and(|runtime| runtime.pending_approval.is_some())
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers, MouseEvent};
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use tempfile::TempDir;

    use super::*;

    use crate::session::{
        MAX_THINKING_BUFFER_BYTES, MAX_THINKING_LINE_BYTES, estimate_context_tokens,
        next_output_scroll_top, trim_entries,
    };

    fn handle_event_for_test(app: &mut App, event: AgentEvent) -> crate::session::SessionOutcome {
        let ctx = crate::session::EventCtx {
            storage: &app.storage,
            workspace: &app.workspace,
        };
        app.current.handle_event(&ctx, event)
    }

    #[test]
    fn next_mode_cycles_in_build_plan_explore_cluster_order() {
        assert_eq!(next_mode(AgentMode::Build), AgentMode::Plan);
        assert_eq!(next_mode(AgentMode::Plan), AgentMode::Explore);
        assert_eq!(next_mode(AgentMode::Explore), AgentMode::Cluster);
        assert_eq!(next_mode(AgentMode::Cluster), AgentMode::Build);
    }

    #[test]
    fn home_actions_create_only_explicit_new_sessions() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();
        let storage = Storage::open(&temp.path().join("agent.db")).unwrap();

        assert!(
            resolve_home_action(&storage, &workspace, HomeAction::Quit)
                .unwrap()
                .is_none()
        );
        assert!(storage.list_sessions(&workspace).unwrap().is_empty());

        let (created, prompt) =
            resolve_home_action(&storage, &workspace, HomeAction::StartNew("hello".into()))
                .unwrap()
                .unwrap();
        assert_eq!(prompt.as_deref(), Some("hello"));
        assert_eq!(storage.list_sessions(&workspace).unwrap().len(), 1);

        let mut config = Config::default();
        apply_home_selection(
            &mut config,
            &storage,
            &created,
            HomeSelection {
                provider: ProviderPreset::DeepSeek.defaults(),
                mode: AgentMode::Explore,
            },
        )
        .unwrap();
        assert_eq!(config.provider.preset, ProviderPreset::DeepSeek);
        assert_eq!(storage.session_mode(&created).unwrap(), "explore");

        let (resumed, prompt) =
            resolve_home_action(&storage, &workspace, HomeAction::Resume(created.clone()))
                .unwrap()
                .unwrap();
        assert_eq!(resumed, created);
        assert!(prompt.is_none());
        assert_eq!(storage.list_sessions(&workspace).unwrap().len(), 1);
    }

    #[test]
    fn submit_keeps_first_prompt_when_provider_is_unavailable() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.current.runner = None;
        app.input.set("keep this prompt");

        submit_input(&mut app).unwrap();

        assert_eq!(app.input.as_str(), "keep this prompt");
        assert_eq!(app.current.status, "请打开提供商设置配置 API Key");
    }

    #[tokio::test]
    async fn delete_last_session_creates_replacement_and_removes_old_runtime() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let deleted = app.active_session.clone();

        execute_command(&mut app, Command::Delete).unwrap();

        assert_ne!(app.active_session, deleted);
        assert_eq!(app.current.session_id, app.active_session);
        assert!(!app.background.contains_key(&deleted));
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].id, app.active_session);
        assert_eq!(app.current.mode, AgentMode::Build);
        assert_eq!(app.current.status, "会话已删除");
        let sessions = app.storage.list_sessions(&app.workspace).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, app.active_session);
    }

    #[tokio::test]
    async fn delete_session_switches_to_most_recent_remaining_session() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let deleted = app.active_session.clone();
        let replacement = app.storage.create_session(&app.workspace).unwrap();

        execute_command(&mut app, Command::Delete).unwrap();

        assert_eq!(app.active_session, replacement);
        assert_eq!(app.current.session_id, replacement);
        assert!(!app.background.contains_key(&deleted));
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].id, replacement);
        assert_eq!(app.current.status, "会话已删除");
    }

    #[test]
    fn rename_command_requires_title_and_updates_current_session() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let original_title = app.sessions[0].title.clone();

        execute_palette_action(&mut app, commands::PaletteAction::Command("/rename")).unwrap();

        assert_eq!(app.input.as_str(), "/rename ");
        assert_eq!(app.sessions[0].title, original_title);
        assert_eq!(
            app.storage.list_sessions(&app.workspace).unwrap()[0]
                .title
                .clone(),
            original_title
        );
        assert!(app.current.status.contains("新会话名称"));

        execute_command(&mut app, Command::Rename(Some("  新名称  ".into()))).unwrap();

        assert_eq!(app.sessions[0].title, "新名称");
        assert_eq!(
            app.storage.list_sessions(&app.workspace).unwrap()[0]
                .title
                .clone(),
            "新名称"
        );
        assert_eq!(app.current.status, "会话已重命名为 新名称");
    }

    #[test]
    fn export_session_defaults_to_a_visible_workspace_file() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.current.conversation.push(ConversationItem::Message {
            role: Role::User,
            content: "export me".into(),
        });
        let session_id = app.current.session_id.clone();

        export_session(&mut app, None).unwrap();

        let target = app.workspace.join(format!("1h-agent-{session_id}.md"));
        assert!(target.is_file());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "## You\n\nexport me\n\n"
        );
        assert!(app.current.status.contains("工作区"));
        assert!(
            app.current
                .status
                .contains(&format!("1h-agent-{session_id}.md"))
        );
    }

    #[test]
    fn export_session_accepts_a_workspace_relative_path() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);

        export_session(&mut app, Some("conversation.md".into())).unwrap();

        let target = app.workspace.join("conversation.md");
        assert!(target.is_file());
        assert!(app.current.status.contains("conversation.md"));
    }

    #[tokio::test]
    async fn undo_and_redo_reload_the_active_session_history() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let session_id = app.current.session_id.clone();
        app.storage
            .append_message(&session_id, Role::User, "hello")
            .unwrap();
        app.storage
            .append_message(&session_id, Role::Assistant, "hi")
            .unwrap();
        app.storage
            .save_response_id(&session_id, "response")
            .unwrap();

        execute_command(&mut app, Command::Undo).unwrap();

        assert_eq!(app.current.status, "已撤销上一轮");
        assert!(app.current.conversation.is_empty());
        assert_eq!(app.current.entries.len(), 1);
        assert!(app.storage.response_id(&session_id).unwrap().is_none());

        execute_command(&mut app, Command::Redo).unwrap();

        assert_eq!(app.current.status, "已重做上一轮");
        assert_eq!(app.current.conversation.len(), 2);
        assert_eq!(app.current.entries.len(), 2);
    }

    #[test]
    fn switch_mode_updates_registry_storage_and_status() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.storage
            .save_response_id(&app.current.session_id, "stale-response")
            .unwrap();
        switch_mode(&mut app, AgentMode::Explore).unwrap();
        assert_eq!(app.current.mode, AgentMode::Explore);
        assert!(app.current.status.contains("EXPLORE"));
        assert_eq!(
            app.storage.session_mode(&app.current.session_id).unwrap(),
            AgentMode::Explore.as_str()
        );
        assert!(
            app.storage
                .response_id(&app.current.session_id)
                .unwrap()
                .is_none()
        );
    }

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

    #[test]
    fn palette_actions_dispatch_commands_and_cycle_mode() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        execute_palette_action(&mut app, commands::PaletteAction::Command("/provider")).unwrap();
        assert!(app.settings.is_some());

        let mut app = test_app(&temp);
        execute_palette_action(&mut app, commands::PaletteAction::CycleMode).unwrap();
        assert_eq!(app.current.mode, AgentMode::Plan);
    }

    #[tokio::test]
    async fn ctrl_p_and_ctrl_x_open_the_same_palette() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        )
        .await
        .unwrap();
        assert!(app.palette.is_some());
        assert!(app.current.status.contains("↑/↓"));

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        assert!(app.palette.is_none());

        handle_terminal_event(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
        )
        .await
        .unwrap();
        assert!(app.palette.is_some());
    }

    #[test]
    fn palette_selection_changes_the_visible_description() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        open_palette(&mut app);
        let matches = commands::matches("", 10);
        let first = commands::PALETTE_ITEMS[matches[0].index].description;
        let first_screen = render_screen(&mut app, 120, 30).join("\n");
        assert!(first_screen.replace(' ', "").contains(first));
        handle_palette_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let selected = app.palette.as_ref().unwrap().selected;
        let selected_description = commands::PALETTE_ITEMS[matches[selected].index].description;
        assert_ne!(first, selected_description);
        let selected_screen = render_screen(&mut app, 120, 30).join("\n");
        assert!(
            selected_screen
                .replace(' ', "")
                .contains(selected_description)
        );
    }

    #[test]
    fn palette_enter_executes_the_selected_command() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        open_palette(&mut app);
        app.palette.as_mut().unwrap().query = "quit".into();
        handle_palette_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.should_quit);
        assert!(app.palette.is_none());
    }

    #[test]
    fn settings_paste_inserts_into_text_fields_and_strips_newlines() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        open_settings(&mut app);
        let provider = app.config.provider.clone();
        open_provider_form(&mut app, provider);

        let form = app.settings.as_mut().unwrap().form_mut().unwrap();
        form.selected = 3; // BaseUrl
        assert!(paste_text_into_settings(
            &mut app,
            "https://api.example.com/v1\n"
        ));
        assert_eq!(
            app.settings
                .as_ref()
                .unwrap()
                .form()
                .unwrap()
                .provider
                .base_url,
            "https://api.example.com/v1"
        );

        let form = app.settings.as_mut().unwrap().form_mut().unwrap();
        form.selected = 5; // ApiKey
        assert!(paste_text_into_settings(&mut app, "sk-test-123\r\n"));
        assert_eq!(
            app.settings.as_ref().unwrap().form().unwrap().api_key,
            "sk-test-123"
        );

        let form = app.settings.as_mut().unwrap().form_mut().unwrap();
        form.selected = 0; // Preset is not editable text
        assert!(!paste_text_into_settings(&mut app, "deepseek"));
    }

    #[test]
    fn settings_paste_shortcut_is_handled() {
        assert!(settings_key_handled(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        ));
        assert!(settings_key_handled(
            KeyCode::Char('v'),
            KeyModifiers::SUPER
        ));
    }

    #[test]
    fn provider_list_mouse_opens_profile_and_add_template_picker() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.config
            .upsert_provider(ProviderPreset::OpenAi.defaults());
        open_settings(&mut app);
        app.settings_rect = Some(Rect::new(10, 10, 70, 20));

        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row,
            modifiers: KeyModifiers::NONE,
        };
        handle_settings_mouse(&mut app, click(13)).unwrap();
        assert!(matches!(app.settings, Some(SettingsState::Form(_))));

        reopen_provider_list(&mut app);
        app.settings_rect = Some(Rect::new(10, 10, 70, 20));
        handle_settings_mouse(&mut app, click(15)).unwrap();
        assert!(matches!(app.settings, Some(SettingsState::Templates(_))));
    }

    #[test]
    fn provider_model_click_opens_picker_and_applies_clicked_model() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.model_control_rect = Some(Rect::new(20, 20, 22, 1));
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 20,
            row,
            modifiers: KeyModifiers::NONE,
        };

        handle_model_mouse(&mut app, click(20)).unwrap();
        assert!(app.model_menu_open);
        assert_eq!(model_choices(&app).first().unwrap(), "gpt-5-mini");

        app.model_menu_rect = Some(Rect::new(10, 10, 40, 8));
        handle_model_mouse(&mut app, click(12)).unwrap();
        assert!(!app.model_menu_open);
        assert_eq!(app.config.provider.model, "gpt-5");
        assert_eq!(
            app.config
                .provider_for(ProviderPreset::OpenAi)
                .unwrap()
                .model,
            "gpt-5"
        );
    }

    #[test]
    fn provider_and_model_text_open_independent_pickers() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.config
            .upsert_provider(ProviderPreset::OpenAi.defaults());
        app.config.upsert_provider(ProviderPreset::Qwen.defaults());
        app.active_secret = Some((ProviderPreset::Qwen, "test-key".into()));
        app.provider_control_rect = Some(Rect::new(10, 20, 6, 1));
        app.model_control_rect = Some(Rect::new(19, 20, 10, 1));
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        handle_provider_mouse(&mut app, click(12, 20)).unwrap();
        assert!(app.provider_menu_open);
        assert!(!app.model_menu_open);
        assert_eq!(
            provider_choices(&app),
            vec![ProviderPreset::OpenAi, ProviderPreset::Qwen]
        );

        app.provider_menu_rect = Some(Rect::new(10, 10, 30, 4));
        handle_provider_mouse(&mut app, click(12, 12)).unwrap();
        assert!(!app.provider_menu_open);
        assert_eq!(app.config.provider.preset, ProviderPreset::Qwen);
        assert_eq!(
            app.config.provider.model,
            ProviderPreset::Qwen.defaults().model
        );

        handle_model_mouse(&mut app, click(20, 20)).unwrap();
        assert!(app.model_menu_open);
        assert!(!app.provider_menu_open);
    }

    #[test]
    fn cluster_command_switches_to_cluster_mode() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        assert_eq!(app.current.mode, AgentMode::Build);
        execute_command(&mut app, Command::Mode(AgentMode::Cluster)).unwrap();
        assert_eq!(app.current.mode, AgentMode::Cluster);
    }

    #[tokio::test]
    async fn activate_session_keeps_global_model_when_session_model_is_empty() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.config.provider.preset = ProviderPreset::DeepSeek;
        app.config.provider.model = "deepseek-v4-flash".into();
        let new_session = app.storage.create_session(&app.workspace).unwrap();
        activate_session(&mut app, new_session).unwrap();
        // A regular session stores an empty model; it must fall back to the
        // global DeepSeek model (deepseek-v4-flash window) rather than "".
        assert_eq!(app.current.context_limit_tokens, Some(1_000_000));
    }

    #[tokio::test]
    async fn handle_routed_event_records_child_session_status() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let parent = app.active_session.clone();
        let child_id = app
            .storage
            .create_child_session(
                &app.workspace,
                &parent,
                "openai",
                "gpt-5-mini",
                "child",
                "explore",
                "reviewer",
            )
            .unwrap();
        let redraw = handle_routed_event(
            &mut app,
            RoutedEvent {
                session_id: parent,
                event: AgentEvent::ChildSessionProgress {
                    session_id: child_id.clone(),
                    progress: ChildSessionProgress {
                        status: ChildSessionStatus::WaitingModel,
                        turn: 1,
                        max_turns: 3,
                        tool: None,
                        updated_at: Instant::now(),
                    },
                },
            },
        );
        assert!(redraw);
        assert_eq!(
            app.child_status
                .get(&child_id)
                .map(|progress| progress.status),
            Some(ChildSessionStatus::WaitingModel)
        );
    }

    #[tokio::test]
    async fn cluster_batch_status_tracks_queued_running_and_completed_children() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let parent = app.active_session.clone();
        let child_a = app
            .storage
            .create_child_session(
                &app.workspace,
                &parent,
                "openai",
                "gpt-5-mini",
                "child-a",
                "explore",
                "reviewer",
            )
            .unwrap();
        let child_b = app
            .storage
            .create_child_session(
                &app.workspace,
                &parent,
                "openai",
                "gpt-5-mini",
                "child-b",
                "explore",
                "reviewer",
            )
            .unwrap();
        let route = |app: &mut App, child: &str, status| {
            handle_routed_event(
                app,
                RoutedEvent {
                    session_id: parent.clone(),
                    event: AgentEvent::ChildSessionProgress {
                        session_id: child.into(),
                        progress: ChildSessionProgress {
                            status,
                            turn: 1,
                            max_turns: 3,
                            tool: None,
                            updated_at: Instant::now(),
                        },
                    },
                },
            )
        };

        assert!(route(&mut app, &child_a, ChildSessionStatus::Queued));
        assert!(route(&mut app, &child_b, ChildSessionStatus::Queued));
        assert!(route(&mut app, &child_a, ChildSessionStatus::WaitingModel));
        assert_eq!(app.current.status, "集群 0/2 完成 · 1 运行 · 1 排队");
        assert!(route(&mut app, &child_a, ChildSessionStatus::Completed));
        assert_eq!(app.current.status, "集群 1/2 完成 · 0 运行 · 1 排队");
    }

    #[tokio::test]
    async fn background_child_approval_is_globally_visible_and_routed() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let owner = app.active_session.clone();
        let other = app.storage.create_session(&app.workspace).unwrap();
        activate_session(&mut app, other).unwrap();

        let (reply, answer) = oneshot::channel();
        app.background.get_mut(&owner).unwrap().pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "background-approval".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/lib.rs"}),
            },
            reason: "test background routing".into(),
            source_session_id: Some("child-session".into()),
            source_title: Some("background-child".into()),
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });

        assert!(app.has_pending_approval());
        assert_eq!(
            app.pending_approval()
                .and_then(|approval| approval.source_title.as_deref()),
            Some("background-child")
        );
        let screen = render_screen(&mut app, 80, 24);
        assert!(screen.iter().any(|line| line.contains("background-child")));
        resolve_approval(&mut app, true);
        assert!(answer.await.unwrap());
        assert!(!app.has_pending_approval());
        assert_eq!(
            app.background.get(&owner).unwrap().status,
            "已批准，开始执行工具……"
        );
    }

    #[tokio::test]
    async fn switching_session_parks_runtime_and_switches_back() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let old_session = app.active_session.clone();
        let new_session = app.storage.create_session(&app.workspace).unwrap();

        activate_session(&mut app, new_session.clone()).unwrap();
        assert_eq!(app.active_session, new_session);
        assert!(app.background.contains_key(&old_session));

        activate_session(&mut app, old_session.clone()).unwrap();
        assert_eq!(app.active_session, old_session);
        assert!(app.background.contains_key(&new_session));
    }

    #[tokio::test]
    async fn delete_running_session_aborts_task_and_rejects_approval() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let deleted = app.active_session.clone();
        let (task_finished, task_result) = oneshot::channel();
        app.current.busy = true;
        app.current.active_task = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = task_finished.send(());
        }));
        let (approval_reply, approval_result) = oneshot::channel();
        app.current.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "delete-running".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/lib.rs"}),
            },
            reason: "test deletion shutdown".into(),
            source_session_id: None,
            source_title: None,
            action: ApprovalAction::Agent(approval_reply),
            created_at: Instant::now(),
        });

        execute_command(&mut app, Command::Delete).unwrap();

        assert!(!app.background.contains_key(&deleted));
        assert!(!approval_result.await.unwrap());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), task_result)
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn delete_parent_shutdowns_descendant_runtimes_and_tracking() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let parent = app.active_session.clone();
        let child = app
            .storage
            .create_child_session(
                &app.workspace,
                &parent,
                "openai",
                "gpt-5-mini",
                "child",
                "explore",
                "reviewer",
            )
            .unwrap();
        activate_session(&mut app, child.clone()).unwrap();
        activate_session(&mut app, parent.clone()).unwrap();
        let (approval_reply, approval_result) = oneshot::channel();
        app.background.get_mut(&child).unwrap().pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "descendant-approval".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/lib.rs"}),
            },
            reason: "test descendant shutdown".into(),
            source_session_id: Some(child.clone()),
            source_title: Some("child".into()),
            action: ApprovalAction::Agent(approval_reply),
            created_at: Instant::now(),
        });
        app.child_status.insert(
            child.clone(),
            ChildSessionProgress {
                status: ChildSessionStatus::WaitingApproval,
                turn: 1,
                max_turns: 1,
                tool: Some("file_write".into()),
                updated_at: Instant::now(),
            },
        );
        app.child_batches
            .insert(parent.clone(), HashSet::from([child.clone()]));
        app.expanded_sessions
            .extend([parent.clone(), child.clone()]);

        execute_command(&mut app, Command::Delete).unwrap();

        assert!(!app.background.contains_key(&parent));
        assert!(!app.background.contains_key(&child));
        assert!(!app.child_status.contains_key(&child));
        assert!(!app.child_batches.contains_key(&parent));
        assert!(!app.expanded_sessions.contains(&parent));
        assert!(!app.expanded_sessions.contains(&child));
        assert!(!approval_result.await.unwrap());
    }

    #[tokio::test]
    async fn background_capacity_evicts_least_recently_parked_idle_runtime() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.config.runtime.max_background_sessions = 2;
        let first = app.active_session.clone();
        let second = app.storage.create_session(&app.workspace).unwrap();
        let third = app.storage.create_session(&app.workspace).unwrap();
        let fourth = app.storage.create_session(&app.workspace).unwrap();

        activate_session(&mut app, second.clone()).unwrap();
        app.background.get_mut(&first).unwrap().parked_at = Instant::now();
        activate_session(&mut app, third.clone()).unwrap();
        app.background.get_mut(&second).unwrap().parked_at =
            Instant::now() + Duration::from_secs(1);
        activate_session(&mut app, fourth).unwrap();

        assert_eq!(app.background.len(), 2);
        assert!(!app.background.contains_key(&first));
        assert!(app.background.contains_key(&second));
        assert!(app.background.contains_key(&third));
    }

    #[tokio::test]
    async fn background_capacity_protects_busy_and_approval_runtimes() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.config.runtime.max_background_sessions = 2;
        let busy = app.active_session.clone();
        let waiting = app.storage.create_session(&app.workspace).unwrap();
        let evicted = app.storage.create_session(&app.workspace).unwrap();
        let active = app.storage.create_session(&app.workspace).unwrap();

        activate_session(&mut app, waiting.clone()).unwrap();
        app.background.get_mut(&busy).unwrap().busy = true;
        activate_session(&mut app, evicted.clone()).unwrap();
        let (approval_reply, approval_result) = oneshot::channel();
        app.background.get_mut(&waiting).unwrap().pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "capacity-approval".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/lib.rs"}),
            },
            reason: "test protected approval".into(),
            source_session_id: None,
            source_title: None,
            action: ApprovalAction::Agent(approval_reply),
            created_at: Instant::now(),
        });
        activate_session(&mut app, active).unwrap();

        assert_eq!(app.background.len(), 2);
        assert!(app.background.contains_key(&busy));
        assert!(app.background.contains_key(&waiting));
        assert!(!app.background.contains_key(&evicted));
        app.background.get_mut(&waiting).unwrap().shutdown();
        assert!(!approval_result.await.unwrap());
    }

    #[tokio::test]
    async fn background_capacity_stops_oldest_busy_runtime_when_required() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.config.runtime.max_background_sessions = 2;
        let oldest = app.active_session.clone();
        let second = app.storage.create_session(&app.workspace).unwrap();
        let third = app.storage.create_session(&app.workspace).unwrap();
        let active = app.storage.create_session(&app.workspace).unwrap();

        activate_session(&mut app, second.clone()).unwrap();
        let (approval_reply, approval_result) = oneshot::channel();
        app.background.get_mut(&oldest).unwrap().pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "forced-capacity-approval".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/lib.rs"}),
            },
            reason: "test strict capacity".into(),
            source_session_id: None,
            source_title: None,
            action: ApprovalAction::Agent(approval_reply),
            created_at: Instant::now(),
        });
        activate_session(&mut app, third.clone()).unwrap();
        app.background.get_mut(&second).unwrap().busy = true;
        app.current.busy = true;

        activate_session(&mut app, active).unwrap();

        assert_eq!(app.background.len(), 2);
        assert!(!app.background.contains_key(&oldest));
        assert!(app.background.contains_key(&second));
        assert!(app.background.contains_key(&third));
        assert!(!approval_result.await.unwrap());
    }

    fn thinking_summary_count(app: &App) -> usize {
        app.current
            .entries
            .iter()
            .filter(|entry| matches!(entry.kind, DisplayKind::Thinking))
            .count()
    }

    fn last_thinking_summary(app: &App) -> Option<&str> {
        app.current
            .entries
            .iter()
            .rev()
            .find(|entry| matches!(entry.kind, DisplayKind::Thinking))
            .and_then(|entry| match &entry.content {
                DisplayContent::Thinking(thinking) => Some(thinking.content.as_str()),
                _ => None,
            })
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
        let (agent_tx, _agent_rx) = mpsc::channel(8);
        let (router_tx, router_rx) = mpsc::channel(16);
        let approval_lock = Arc::new(Mutex::new(()));
        let runtime = SessionRuntime {
            session_id: session_id.clone(),
            status: String::new(),
            entries: vec![DisplayEntry {
                kind: DisplayKind::Assistant,
                content: DisplayContent::Markdown("first line\n\n中文 🙂 long output".into()),
            }],
            todos: Vec::new(),
            busy: false,
            agent_phase: AgentPhase::Idle,
            model_phase: ModelPhase::Idle,
            thinking_last_line: String::new(),
            thinking_active: false,
            thinking_buffer: String::new(),
            thinking_buffer_truncated: false,
            thinking_buffer_epoch: 0,
            live_thinking_layout_cache: Default::default(),
            thinking_animation_frame: 0,
            thinking_anchor: None,
            thinking_result: ThinkingResult::Completed,
            usage: Usage::default(),
            context_used_tokens: 1,
            context_limit_tokens: None,
            pending_approval: None,
            mode: AgentMode::default(),
            child_role: None,
            expanded_tools: HashSet::new(),
            expanded_thinking: HashSet::new(),
            thinking_expanded: false,
            message_scroll: 0,
            follow_output: true,
            output_scroll_top: None,
            output_selection: None,
            message_layout: None,
            markdown_render_cache: HashMap::new(),
            output_layout_dirty: true,
            output_layout_rebuild_count: 0,
            markdown_parse_count: 0,
            footer_rebuild_count: 0,
            edge_scroll: EdgeScroll::default(),
            conversation: Vec::new(),
            runner: None,
            agent_tx,
            active_task: None,
            parked_at: Instant::now(),
        };
        App {
            workspace,
            input: InputBuffer::new(),
            context_meter_enabled: false,
            settings: None,
            settings_rect: None,
            palette: None,
            thinking_menu_open: false,
            thinking_control_rect: None,
            thinking_menu_rect: None,
            session_panel_rect: None,
            input_mode_rect: None,
            provider_control_rect: None,
            model_control_rect: None,
            provider_menu_open: false,
            provider_menu_rect: None,
            provider_menu_selected: 0,
            model_menu_open: false,
            model_menu_rect: None,
            model_menu_selected: 0,
            force_full_redraw: false,
            mouse_press_target: None,
            mouse_press_position: None,
            mouse_dragged: false,
            layout_restore_anchor: None,
            file_suggestions: Vec::new(),
            file_selected: 0,
            sessions,
            expanded_sessions: HashSet::new(),
            child_status: HashMap::new(),
            child_batches: HashMap::new(),
            storage,
            config,
            registry,
            approval_lock,
            active_secret: None,
            active_session: session_id,
            current: runtime,
            background: HashMap::new(),
            router_tx,
            router_rx,
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

        app.current.agent_phase = AgentPhase::StreamingText;
        assert_eq!(activity_view(&app).text, "正在生成回复");
        app.current.agent_phase = AgentPhase::Thinking;
        assert_eq!(activity_view(&app).text, "正在思考");
        app.current.agent_phase = AgentPhase::ToolRunning;
        assert!(activity_view(&app).text.starts_with("正在执行："));
        app.current.agent_phase = AgentPhase::Failed;
        assert_eq!(activity_view(&app).state, ActivityState::Failed);

        let (reply, _receiver) = oneshot::channel();
        app.current.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "approval".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/ui.rs"}),
            },
            reason: "test".into(),
            source_session_id: None,
            source_title: None,
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
        app.current.context_used_tokens = 87_000;
        app.current.context_limit_tokens = Some(258_000);

        let wide = render_screen(&mut app, 120, 30);
        assert!(wide[28].contains('○'));
        assert!(wide[28].contains("Enter"));
        assert!(wide[28].contains("Ctrl+P"));
        assert!(wide[29].contains("OpenAI"));
        assert!(wide[29].contains("33%"));
        assert!(wide[29].contains("87k/258k"));
        assert!(wide.iter().any(|line| line.contains("Alt+Up/Down")));

        app.current.busy = true;
        app.current.agent_phase = AgentPhase::Thinking;
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
        let rebuilds = app.current.output_layout_rebuild_count;
        let parses = app.current.markdown_parse_count;
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
        assert!(app.current.status.contains("配置保存失败"));
        assert_eq!(app.current.output_layout_rebuild_count, rebuilds);
        assert_eq!(app.current.markdown_parse_count, parses);

        app.current.busy = true;
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
            app.current.pending_approval = Some(PendingApproval {
                call: ToolCall {
                    id: "approval".into(),
                    name: "file_write".into(),
                    arguments: serde_json::json!({"path":"src/ui.rs"}),
                },
                reason: "risk text".into(),
                source_session_id: None,
                source_title: None,
                action: ApprovalAction::Agent(reply),
                created_at: Instant::now(),
            });
            resolve_approval(&mut app, approved);
            assert!(app.current.pending_approval.is_none());
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
        app.current.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "cancel".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({}),
            },
            reason: "cancel".into(),
            source_session_id: None,
            source_title: None,
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });
        cancel_active_request(&mut app);
        assert!(app.force_full_redraw);

        let (reply, _receiver) = oneshot::channel();
        app.current.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "event-cancel".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({}),
            },
            reason: "cancel".into(),
            source_session_id: None,
            source_title: None,
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });
        app.force_full_redraw = false;
        let outcome = handle_event_for_test(&mut app, AgentEvent::Cancelled("cancelled".into()));
        assert!(outcome.force_redraw);
    }

    #[test]
    fn approval_overlay_clear_restores_underlying_frame() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let (reply, _receiver) = oneshot::channel();
        app.current.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "approval".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/ui.rs"}),
            },
            reason: "unique-risk-text".into(),
            source_session_id: None,
            source_title: None,
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
        app.current.scroll_messages(1);
        assert!(!app.force_full_redraw);
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("真实思考".into()));
        handle_event_for_test(&mut app, AgentEvent::TextDelta("正文".into()));
        assert!(!app.force_full_redraw);
    }

    #[test]
    fn footer_updates_do_not_rebuild_messages_or_parse_markdown() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        render_screen(&mut app, 80, 20);
        let layout_rebuilds = app.current.output_layout_rebuild_count;
        let markdown_parses = app.current.markdown_parse_count;
        let footer_rebuilds = app.current.footer_rebuild_count;

        app.current.status = "仅 Footer 变化".into();
        render_screen(&mut app, 80, 20);
        assert_eq!(app.current.output_layout_rebuild_count, layout_rebuilds);
        assert_eq!(app.current.markdown_parse_count, markdown_parses);
        assert_eq!(app.current.footer_rebuild_count, footer_rebuilds + 1);
    }

    #[test]
    fn footer_keeps_context_visible_when_model_name_is_long() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.context_meter_enabled = true;
        app.current.context_used_tokens = 87_000;
        app.current.context_limit_tokens = Some(258_000);
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
        app.current.pending_approval = Some(PendingApproval {
            call: ToolCall {
                id: "approval-screen".into(),
                name: "file_write".into(),
                arguments: serde_json::json!({"path":"src/ui.rs"}),
            },
            reason: "将修改工作区文件".into(),
            source_session_id: None,
            source_title: None,
            action: ApprovalAction::Agent(reply),
            created_at: Instant::now(),
        });
        let approval = render_screen(&mut app, 100, 24);
        assert!(approval[22].contains('!'));
        assert!(approval[22].contains('Y'));
        assert!(approval[22].contains('N'));
        assert!(approval[23].contains("src/ui.rs"));
        app.current.pending_approval = None;

        app.current.agent_phase = AgentPhase::ToolRunning;
        app.current.entries.push(DisplayEntry {
            kind: DisplayKind::Tool,
            content: DisplayContent::Tool(ToolDisplay {
                call_id: "running-screen".into(),
                name: "file_read".into(),
                arguments: serde_json::json!({"path":"src/app.rs"}),
                status: ToolDisplayStatus::Running,
                result: None,
            }),
        });
        app.current.invalidate_output_layout();
        let tool = render_screen(&mut app, 100, 24);
        assert!(tool[22].contains('●'));
        assert!(tool.iter().any(|line| line.contains("src/app.rs")));

        app.current.agent_phase = AgentPhase::Failed;
        let failed = render_screen(&mut app, 100, 24);
        assert!(failed[22].contains('×'));

        app.context_meter_enabled = true;
        app.current.context_limit_tokens = Some(100);
        for (used, role) in [
            (70, VisualRole::Secondary),
            (85, VisualRole::Warning),
            (95, VisualRole::Danger),
        ] {
            app.current.context_used_tokens = used;
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
        assert_eq!(app.current.output_layout_rebuild_count, 1);
        let markdown_parses = app.current.markdown_parse_count;

        let layout = app.current.message_layout.as_ref().unwrap();
        let text_ptr = layout.text.as_ptr();
        let lines_ptr = layout.lines.as_ptr();
        let visual_lines_ptr = layout.visual_lines.as_ptr();
        let line_count = layout.lines.len();
        let visual_line_count = layout.visual_lines.len();
        app.current.scroll_messages(1);
        crate::ui::update_message_layout(&mut app, viewport);
        app.current.scroll_messages(-1);
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 8, 5));

        let layout = app.current.message_layout.as_ref().unwrap();
        assert_eq!(app.current.output_layout_rebuild_count, 1);
        assert_eq!(app.current.markdown_parse_count, markdown_parses);
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
        let text_ptr = app.current.message_layout.as_ref().unwrap().text.as_ptr();
        let lines_ptr = app.current.message_layout.as_ref().unwrap().lines.as_ptr();
        let markdown_parses = app.current.markdown_parse_count;

        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 16, 3));
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 16, 3));
        let layout = app.current.message_layout.as_ref().unwrap();
        assert_eq!(app.current.output_layout_rebuild_count, 2);
        assert_eq!(app.current.markdown_parse_count, markdown_parses);
        assert_eq!(layout.text.as_ptr(), text_ptr);
        assert_eq!(layout.lines.as_ptr(), lines_ptr);
    }

    #[test]
    fn text_delta_invalidates_and_rebuilds_with_new_text() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 4);
        crate::ui::update_message_layout(&mut app, viewport);
        handle_event_for_test(&mut app, AgentEvent::TextDelta(" NEW".into()));
        assert!(app.current.output_layout_dirty);
        assert!(app.current.message_layout.is_none());

        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.current.output_layout_rebuild_count, 2);
        assert!(
            app.current
                .message_layout
                .as_ref()
                .unwrap()
                .text
                .contains("NEW")
        );
    }

    #[test]
    fn reasoning_deltas_keep_only_latest_line_out_of_history_and_storage() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 4);
        crate::ui::update_message_layout(&mut app, viewport);
        let entries = app.current.entries.len();
        let stored = app.storage.load_messages(&app.current.session_id).unwrap();
        let layout_text = app.current.message_layout.as_ref().unwrap().text.clone();
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        assert!(app.current.thinking_active);
        assert_eq!(app.current.thinking_last_line, "模型正在思考");
        crate::ui::update_message_layout(&mut app, viewport);
        let rebuilds = app.current.output_layout_rebuild_count;
        let markdown_parses = app.current.markdown_parse_count;
        handle_event_for_test(
            &mut app,
            AgentEvent::ReasoningDelta("第一行\n\n最新".into()),
        );
        assert_eq!(
            crate::ui::live_thinking_line_with_braille(&app, true),
            "⠋ 思考中  最新"
        );
        crate::ui::update_message_layout(&mut app, viewport);
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("一行".into()));

        assert_eq!(app.current.thinking_last_line, "最新一行");
        assert_eq!(
            crate::ui::live_thinking_line_with_braille(&app, true),
            "⠋ 思考中  最新一行"
        );
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.current.entries.len(), entries);
        assert_eq!(
            app.storage.load_messages(&app.current.session_id).unwrap(),
            stored
        );
        assert_eq!(
            app.current.message_layout.as_ref().unwrap().text,
            layout_text
        );
        assert!(
            !app.current
                .message_layout
                .as_ref()
                .unwrap()
                .text
                .contains("最新一行")
        );
        let layout = app.current.message_layout.as_ref().unwrap();
        let copied = layout
            .selected_text(OutputSelection {
                anchor: 0,
                active: layout.text.len(),
                dragging: false,
            })
            .unwrap();
        assert!(!copied.contains("最新一行"));
        assert_eq!(app.current.output_layout_rebuild_count, rebuilds);
        assert_eq!(app.current.markdown_parse_count, markdown_parses);
    }

    #[test]
    fn finish_thinking_skips_empty_buffer() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(
            &mut app,
            AgentEvent::ToolStarted(ToolCall {
                id: "call-empty".into(),
                name: "file_read".into(),
                arguments: serde_json::json!({"path":"Cargo.toml"}),
            }),
        );
        assert!(app.current.thinking_anchor.is_none());
        assert_eq!(thinking_summary_count(&app), 0);
    }

    #[test]
    fn reasoning_without_newlines_keeps_utf8_safe_tail() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        let delta = format!("{}👩‍💻e\u{301}尾", "中文🙂".repeat(400));
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta(delta));

        assert!(app.current.thinking_last_line.len() <= MAX_THINKING_LINE_BYTES);
        assert!(app.current.thinking_last_line.ends_with("👩‍💻e\u{301}尾"));
        assert!(!app.current.thinking_last_line.contains('\u{fffd}'));
    }

    #[test]
    fn reasoning_terminal_events_set_fixed_statuses_and_persist() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);

        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(
            &mut app,
            AgentEvent::ReasoningDelta("正在分析工具结果".into()),
        );
        handle_event_for_test(&mut app, AgentEvent::TextDelta("answer".into()));
        assert!(!app.current.thinking_active);
        assert!(app.current.thinking_anchor.is_none());
        assert_eq!(app.current.thinking_result, ThinkingResult::Completed);
        assert_eq!(thinking_summary_count(&app), 1);
        assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("正在分析工具结果")));

        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("最后失败位置".into()));
        handle_event_for_test(&mut app, AgentEvent::Failed("failed".into()));
        assert!(!app.current.thinking_active);
        assert!(app.current.thinking_anchor.is_none());
        assert_eq!(app.current.thinking_result, ThinkingResult::Failed);
        assert_eq!(thinking_summary_count(&app), 2);
        assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("最后失败位置")));

        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("取消前内容".into()));
        handle_event_for_test(&mut app, AgentEvent::Cancelled("cancelled".into()));
        assert!(!app.current.thinking_active);
        assert!(app.current.thinking_anchor.is_none());
        assert_eq!(app.current.thinking_result, ThinkingResult::Cancelled);
        assert_eq!(thinking_summary_count(&app), 3);
        assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("取消前内容")));
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
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        crate::ui::update_message_layout(&mut app, viewport);
        let rebuilds = app.current.output_layout_rebuild_count;

        for _ in 0..10 {
            app.current.thinking_animation_frame =
                app.current.thinking_animation_frame.wrapping_add(1);
            crate::ui::update_message_layout(&mut app, viewport);
        }
        assert_eq!(app.current.output_layout_rebuild_count, rebuilds);
    }

    #[test]
    fn tool_rounds_reuse_one_live_thinking_row() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 40, 8);

        for round in 0..3 {
            handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
            handle_event_for_test(
                &mut app,
                AgentEvent::ReasoningDelta(format!("第 {round} 轮")),
            );
            crate::ui::update_message_layout(&mut app, viewport);
            let layout = app.current.message_layout.as_ref().unwrap();
            assert_eq!(layout.live_thinking_rows, 1);
            assert_eq!(
                layout
                    .visual_lines
                    .iter()
                    .filter(|line| line.synthetic)
                    .count(),
                layout.live_thinking_rows
            );
            handle_event_for_test(
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
            app.current
                .message_layout
                .as_ref()
                .unwrap()
                .visual_lines
                .iter()
                .filter(|line| line.synthetic)
                .count(),
            0
        );

        app.current.push_entry(DisplayEntry {
            kind: DisplayKind::User,
            content: DisplayContent::Markdown("next request".into()),
        });
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        crate::ui::update_message_layout(&mut app, viewport);
        let initial_rebuilds = app.current.output_layout_rebuild_count;
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("真实摘要".into()));
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.current.output_layout_rebuild_count, initial_rebuilds);
        let layout = app.current.message_layout.as_ref().unwrap();
        let live_row = layout
            .visual_lines
            .iter()
            .position(|line| line.synthetic)
            .unwrap();
        assert!(layout.position_at_visual_row(live_row, 0).is_none());

        handle_event_for_test(&mut app, AgentEvent::TextDelta("answer".into()));
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(
            app.current.output_layout_rebuild_count,
            initial_rebuilds + 1
        );
        assert_eq!(
            app.current
                .message_layout
                .as_ref()
                .unwrap()
                .live_thinking_before,
            None
        );
        assert_eq!(
            app.current
                .message_layout
                .as_ref()
                .unwrap()
                .visual_lines
                .iter()
                .filter(|line| line.synthetic)
                .count(),
            0
        );
        assert!(app.current.thinking_anchor.is_none());
        assert_eq!(thinking_summary_count(&app), 1);
        assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("真实摘要")));
    }

    #[test]
    fn tool_click_toggle_rebuilds_once_per_toggle() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.current.push_entry(DisplayEntry {
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
            let layout = app.current.message_layout.as_ref().unwrap();
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
            assert!(app.current.output_layout_dirty);
            crate::ui::update_message_layout(&mut app, viewport);
            assert_eq!(app.current.output_layout_rebuild_count, expected_count);
        }
    }

    #[test]
    fn thinking_summary_click_toggles_expansion() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.current.push_entry(DisplayEntry {
            kind: DisplayKind::Thinking,
            content: DisplayContent::Thinking(ThinkingDisplay {
                id: "thinking-0".into(),
                content: "第一行\n最后一行".into(),
            }),
        });
        let viewport = Rect::new(0, 0, 30, 30);
        crate::ui::update_message_layout(&mut app, viewport);
        assert!(app.current.expanded_thinking.is_empty());

        for expected in [true, false] {
            let layout = app.current.message_layout.as_ref().unwrap();
            let target_row = layout
                .visual_lines
                .iter()
                .position(|line| {
                    line.interaction
                        == Some(InteractionTarget::ThinkingSummary("thinking-0".into()))
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
            handle_output_mouse(&mut app, down);
            handle_output_mouse(&mut app, up);
            assert_eq!(
                app.current.expanded_thinking.contains("thinking-0"),
                expected
            );
            crate::ui::update_message_layout(&mut app, viewport);
        }
    }

    #[test]
    fn expanding_thinking_summaries_parses_only_each_target_once() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.current.entries = vec![
            DisplayEntry {
                kind: DisplayKind::Assistant,
                content: DisplayContent::Markdown("**正文**".into()),
            },
            DisplayEntry {
                kind: DisplayKind::Thinking,
                content: DisplayContent::Thinking(ThinkingDisplay {
                    id: "thinking-a".into(),
                    content: "# 摘要 A\n\n正文".into(),
                }),
            },
            DisplayEntry {
                kind: DisplayKind::Thinking,
                content: DisplayContent::Thinking(ThinkingDisplay {
                    id: "thinking-b".into(),
                    content: "# 摘要 B\n\n正文".into(),
                }),
            },
        ];
        app.current.invalidate_output_layout();
        let viewport = Rect::new(0, 0, 40, 20);
        crate::ui::update_message_layout(&mut app, viewport);
        let initial_parses = app.current.markdown_parse_count;

        app.current.expanded_thinking.insert("thinking-a".into());
        app.current.invalidate_output_layout();
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.current.markdown_parse_count, initial_parses + 1);

        app.current.expanded_thinking.remove("thinking-a");
        app.current.invalidate_output_layout();
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.current.markdown_parse_count, initial_parses + 1);

        app.current.expanded_thinking.insert("thinking-b".into());
        app.current.invalidate_output_layout();
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.current.markdown_parse_count, initial_parses + 2);
    }

    #[test]
    fn three_tools_render_as_one_group_and_keep_stable_expansion_ids() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.current.entries = ["file_read", "file_search", "file_write"]
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
        app.current.invalidate_output_layout();
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 80, 20));
        let layout = app.current.message_layout.as_ref().unwrap();
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
        app.current.expanded_tools.insert("call-1".into());
        app.current.entries.insert(
            0,
            DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown("trim me".into()),
            },
        );
        trim_entries(&mut app.current.entries);
        assert!(app.current.expanded_tools.contains("call-1"));
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
        let initial_len = app.current.entries.len();
        handle_event_for_test(&mut app, AgentEvent::ToolStarted(call.clone()));
        handle_event_for_test(
            &mut app,
            AgentEvent::ToolFinished {
                call,
                result: "contents".into(),
            },
        );
        assert_eq!(app.current.entries.len(), initial_len + 1);
        assert!(
            matches!(app.current.entries.last().map(|entry| &entry.content),
            Some(DisplayContent::Tool(tool))
                if tool.status == ToolDisplayStatus::Completed
                    && tool.result.as_deref() == Some("contents"))
        );
    }

    #[test]
    fn clicking_thinking_title_expands_and_collapses_live_rows() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(
            &mut app,
            AgentEvent::ReasoningDelta("第一行\n第二行".into()),
        );
        let viewport = Rect::new(0, 0, 80, 30);
        crate::ui::update_message_layout(&mut app, viewport);
        let thinking_row = app
            .current
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
        assert!(app.current.thinking_expanded);
        assert!(!app.current.output_layout_dirty);
        let rebuilds = app.current.output_layout_rebuild_count;
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(app.current.output_layout_rebuild_count, rebuilds);
        assert!(
            app.current
                .message_layout
                .as_ref()
                .unwrap()
                .live_thinking_rows
                >= 3
        );

        handle_output_mouse(&mut app, click(MouseEventKind::Down(MouseButton::Left)));
        handle_output_mouse(&mut app, click(MouseEventKind::Up(MouseButton::Left)));
        assert!(!app.current.thinking_expanded);
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(
            app.current
                .message_layout
                .as_ref()
                .unwrap()
                .live_thinking_rows,
            1
        );
    }

    #[test]
    fn live_thinking_layout_reuses_body_and_only_processes_appended_tail() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        app.current.thinking_expanded = true;
        let initial = "中文🙂".repeat(4_000);
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta(initial.clone()));
        let viewport = Rect::new(0, 0, 40, 20);
        crate::ui::update_message_layout(&mut app, viewport);
        let rebuilds = app.current.live_thinking_layout_cache.full_rebuilds;
        let processed = app.current.live_thinking_layout_cache.processed_bytes;
        let body_ptr = app
            .current
            .message_layout
            .as_ref()
            .unwrap()
            .live_thinking_lines[1]
            .as_ptr();
        assert_eq!(processed, initial.len());

        app.current.thinking_animation_frame += 1;
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(
            app.current.live_thinking_layout_cache.full_rebuilds,
            rebuilds
        );
        assert_eq!(
            app.current.live_thinking_layout_cache.processed_bytes,
            processed
        );
        assert_eq!(
            app.current
                .message_layout
                .as_ref()
                .unwrap()
                .live_thinking_lines[1]
                .as_ptr(),
            body_ptr
        );

        let tail = "\n尾部👩‍💻";
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta(tail.into()));
        crate::ui::update_message_layout(&mut app, viewport);
        assert_eq!(
            app.current.live_thinking_layout_cache.full_rebuilds,
            rebuilds
        );
        assert_eq!(
            app.current.live_thinking_layout_cache.processed_bytes,
            processed + tail.len()
        );

        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 50, 20));
        assert_eq!(
            app.current.live_thinking_layout_cache.full_rebuilds,
            rebuilds + 1
        );
        assert_eq!(
            app.current.live_thinking_layout_cache.processed_bytes,
            initial.len() + tail.len()
        );

        let rebuilds_after_resize = app.current.live_thinking_layout_cache.full_rebuilds;
        handle_event_for_test(
            &mut app,
            AgentEvent::ReasoningDelta("追加内容🙂".repeat(8_000)),
        );
        assert!(app.current.thinking_buffer_truncated);
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 50, 20));
        assert_eq!(
            app.current.live_thinking_layout_cache.full_rebuilds,
            rebuilds_after_resize + 1
        );
        assert_eq!(
            app.current.live_thinking_layout_cache.processed_bytes,
            app.current.thinking_buffer.trim().len()
        );
    }

    #[test]
    fn stream_redraw_coalescing_is_limited_to_active_text_deltas() {
        let routed = |session_id: &str, event| RoutedEvent {
            session_id: session_id.into(),
            event,
        };
        assert!(should_coalesce_stream_redraw(
            "active",
            &routed("active", AgentEvent::TextDelta("a".into()))
        ));
        assert!(should_coalesce_stream_redraw(
            "active",
            &routed("active", AgentEvent::ReasoningDelta("a".into()))
        ));
        assert!(!should_coalesce_stream_redraw(
            "active",
            &routed("background", AgentEvent::TextDelta("a".into()))
        ));
        assert!(!should_coalesce_stream_redraw(
            "active",
            &routed("active", AgentEvent::ModelStreaming)
        ));
    }

    #[tokio::test]
    async fn deferred_redraw_deadline_is_shared_without_being_reset() {
        let scroll = Event::Mouse(wheel_event(MouseEventKind::ScrollUp));
        assert!(should_coalesce_terminal_redraw(&scroll));
        assert!(!should_coalesce_terminal_redraw(&Event::Mouse(
            wheel_event(MouseEventKind::Down(MouseButton::Left))
        )));
        assert!(!should_coalesce_terminal_redraw(&Event::Key(KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })));

        let mut timer = None;
        schedule_deferred_redraw(&mut timer);
        let first_deadline = timer.as_ref().unwrap().deadline();
        schedule_deferred_redraw(&mut timer);
        assert_eq!(timer.as_ref().unwrap().deadline(), first_deadline);
    }

    #[test]
    fn dragging_from_tool_summary_does_not_toggle_it() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.current.push_entry(DisplayEntry {
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
        let layout = app.current.message_layout.as_ref().unwrap();
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
        assert!(!app.current.expanded_tools.contains("drag-tool"));
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
        assert!(app.current.expanded_tools.is_empty());
    }

    #[test]
    fn thinking_expansion_and_buffer_limit_are_utf8_safe() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(
            &mut app,
            AgentEvent::ReasoningDelta(format!("第一行\n{}尾🙂", "中文👩‍💻".repeat(20_000))),
        );
        assert!(app.current.thinking_buffer.len() <= MAX_THINKING_BUFFER_BYTES);
        assert!(app.current.thinking_buffer_truncated);
        assert!(app.current.thinking_buffer.ends_with("尾🙂"));
        assert!(app.current.thinking_buffer.is_char_boundary(0));
        app.current.thinking_expanded = true;
        let lines = crate::ui::live_thinking_line_with_braille(&app, true);
        assert!(lines.contains("[较早思考内容已截断]"));
        assert!(lines.contains("尾🙂"));
    }

    #[tokio::test]
    async fn clear_and_session_activation_invalidate_layout() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 4);
        crate::ui::update_message_layout(&mut app, viewport);
        execute_command(&mut app, Command::Clear).unwrap();
        assert!(app.current.output_layout_dirty);
        assert!(app.current.message_layout.is_none());

        crate::ui::update_message_layout(&mut app, viewport);
        let session_id = app.storage.create_session(&app.workspace).unwrap();
        activate_session(&mut app, session_id).unwrap();
        assert!(app.current.output_layout_dirty);
        assert!(app.current.message_layout.is_none());
    }

    #[test]
    fn trimming_entries_releases_and_invalidates_layout() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        crate::ui::update_message_layout(&mut app, Rect::new(0, 0, 20, 4));
        app.current.entries.extend((0..1000).map(|_| DisplayEntry {
            kind: DisplayKind::System,
            content: DisplayContent::Markdown("x".into()),
        }));

        app.current.trim_entries();
        assert_eq!(app.current.entries.len(), 1000);
        assert!(app.current.output_layout_dirty);
        assert!(app.current.message_layout.is_none());
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
        let max_scroll = app.current.message_layout.as_ref().unwrap().max_scroll();
        assert!(max_scroll >= 3);
        assert_eq!(app.current.output_layout_rebuild_count, 1);

        assert!(handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollUp)).redraw);
        assert_eq!(app.current.output_scroll_top, Some(max_scroll - 1));
        assert_eq!(app.current.message_scroll, 1);
        assert_eq!(app.current.output_layout_rebuild_count, 1);

        assert!(handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollUp)).redraw);
        assert!(handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollUp)).redraw);
        assert_eq!(app.current.output_scroll_top, Some(max_scroll - 3));
        assert_eq!(app.current.message_scroll, 3);

        assert!(handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollDown)).redraw);
        assert!(handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollDown)).redraw);
        assert_eq!(app.current.output_scroll_top, Some(max_scroll - 1));
        assert_eq!(app.current.message_scroll, 1);
        assert_eq!(app.current.output_layout_rebuild_count, 1);
    }

    #[test]
    fn mouse_wheel_clamps_at_top_and_bottom_one_line_at_a_time() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let viewport = Rect::new(0, 0, 20, 1);
        crate::ui::update_message_layout(&mut app, viewport);
        let max_scroll = app.current.message_layout.as_ref().unwrap().max_scroll();

        app.current.output_scroll_top = Some(0);
        app.current.follow_output = false;
        app.current.message_scroll = max_scroll;
        assert!(!handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollUp)).redraw);
        assert_eq!(app.current.output_scroll_top, Some(0));
        assert_eq!(app.current.message_scroll, max_scroll);

        assert!(handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollDown)).redraw);
        assert_eq!(app.current.output_scroll_top, Some(1));
        assert_eq!(app.current.message_scroll, max_scroll - 1);

        app.current.output_scroll_top = Some(max_scroll);
        app.current.follow_output = false;
        app.current.message_scroll = 0;
        assert!(handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollDown)).redraw);
        assert_eq!(app.current.output_scroll_top, None);
        assert!(app.current.follow_output);
        assert_eq!(app.current.message_scroll, 0);
        assert!(!handle_output_mouse(&mut app, wheel_event(MouseEventKind::ScrollDown)).redraw);
        assert_eq!(app.current.output_layout_rebuild_count, 1);
    }

    #[test]
    fn todo_commands_update_storage_and_runtime() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);

        execute_command(
            &mut app,
            Command::Todo(TodoCommand::Add("write tests".into())),
        )
        .unwrap();
        execute_command(&mut app, Command::Todo(TodoCommand::Doing(1))).unwrap();

        let tasks = app.storage.list_tasks(&app.current.session_id).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "write tests");
        assert_eq!(tasks[0].status, TodoStatus::InProgress);
        assert_eq!(app.current.todos, tasks);
        assert!(app.current.output_layout_dirty);
        assert_eq!(app.current.status, "任务已标记为进行中");
    }

    #[test]
    fn todo_updated_event_updates_runtime_without_rebuilding_entries() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let entries = app.current.entries.len();
        let tasks = vec![TodoTask::new("verify", TodoStatus::Done)];

        handle_event_for_test(&mut app, AgentEvent::TodoUpdated { tasks });

        assert_eq!(app.current.entries.len(), entries);
        assert_eq!(app.current.todos.len(), 1);
        assert_eq!(app.current.todos[0].status, TodoStatus::Done);
        assert!(app.current.output_layout_dirty);
    }

    #[test]
    fn export_includes_todo_checklist() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        execute_command(
            &mut app,
            Command::Todo(TodoCommand::Add("pending task".into())),
        )
        .unwrap();
        execute_command(
            &mut app,
            Command::Todo(TodoCommand::Add("done task".into())),
        )
        .unwrap();
        execute_command(&mut app, Command::Todo(TodoCommand::Done(2))).unwrap();

        export_session(&mut app, None).unwrap();
        let filename = format!("1h-agent-{}.md", app.current.session_id);
        let output = std::fs::read_to_string(app.workspace.join(filename)).unwrap();
        assert!(output.contains("## 任务清单（1/2）"));
        assert!(output.contains("- [ ] pending task"));
        assert!(output.contains("- [x] done task"));
    }

    #[test]
    fn todo_card_renders_and_status_click_cycles() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        app.current.todos = vec![TodoTask::new("clickable", TodoStatus::Pending)];
        app.current.follow_output = false;
        app.current.message_scroll = 0;
        app.current.output_scroll_top = Some(0);

        app.current.todos = (0..10)
            .map(|index| {
                let title = if index == 9 {
                    "x".repeat(80)
                } else {
                    "clickable".to_owned()
                };
                TodoTask::new(title, TodoStatus::Pending)
            })
            .collect();
        let screen = render_screen(&mut app, 20, 30);
        assert!(screen.iter().any(|line| line.contains("0/10")));
        assert!(screen.iter().any(|line| line.contains("○ 1. clickable")));
        let layout = app.current.message_layout.as_ref().unwrap();
        let long_line = layout
            .lines
            .iter()
            .find(|line| line.text.contains("10. "))
            .expect("long todo task should be rendered");
        assert!(unicode_width::UnicodeWidthStr::width(long_line.text.as_str()) <= 20);
        assert!(long_line.text.ends_with("..."));

        let (column, row, target) = {
            let layout = app.current.message_layout.as_ref().unwrap();
            let mut cursor = layout.viewport.y;
            let mut found = None;
            while cursor < layout.viewport.bottom() {
                if let Some(target) = layout.interaction_at(layout.viewport.x, cursor) {
                    found = Some((cursor, target));
                    break;
                }
                cursor += 1;
            }
            let (row, target) = found.expect("todo task should be clickable");
            (layout.viewport.x, row, target)
        };
        assert!(matches!(target, InteractionTarget::Todo(_)));
        let mouse = |kind| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        handle_output_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left)));
        handle_output_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Left)));

        assert_eq!(app.current.todos[0].status, TodoStatus::InProgress);
        assert_eq!(
            app.storage.list_tasks(&app.current.session_id).unwrap()[0].status,
            TodoStatus::InProgress
        );
    }
}
