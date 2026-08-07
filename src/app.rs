use std::{io, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    agent::{AgentEvent, AgentRunner},
    config::{Config, ProviderConfig, ProviderKind, ProviderPreset},
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
    Tool,
    Error,
    System,
}

#[derive(Clone, Debug)]
pub struct DisplayEntry {
    pub kind: DisplayKind,
    pub content: DisplayContent,
}

#[derive(Clone, Debug)]
pub enum DisplayContent {
    Markdown(String),
    ToolCall { name: String, arguments: Value },
    ToolResult { name: String, result: String },
}

pub struct PendingApproval {
    pub call: ToolCall,
    pub reason: String,
    pub reply: oneshot::Sender<bool>,
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

pub struct App {
    pub workspace: PathBuf,
    pub input: String,
    pub entries: Vec<DisplayEntry>,
    pub status: String,
    pub busy: bool,
    pub usage: Usage,
    pub pending_approval: Option<PendingApproval>,
    pub settings: Option<SettingsState>,
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
    let conversation = storage.load_messages(&session_id)?;
    let workspace = Workspace::new(&workspace_path)?;
    let registry = Arc::new(ToolRegistry::new(
        workspace,
        config.runtime.clone(),
        config.security.allow_private_networks,
    ));
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
        Err(_) => (None, None, "Provider configuration required".into()),
    };
    let entries = display_entries(&conversation);
    let mut app = App {
        workspace: workspace_path,
        input: String::new(),
        entries,
        status: initial_status,
        busy: false,
        usage: Usage::default(),
        pending_approval: None,
        settings: None,
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = event_loop(&mut terminal, &mut app).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    terminal.draw(|frame| ui::draw(frame, app))?;

    while !app.should_quit {
        tokio::select! {
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(event)) => handle_terminal_event(app, event).await?,
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
        terminal.draw(|frame| ui::draw(frame, app))?;
    }
    if let Some(task) = app.active_task.take() {
        task.abort();
    }
    if let Some(approval) = app.pending_approval.take() {
        let _ = approval.reply.send(false);
    }
    Ok(())
}

async fn handle_terminal_event(app: &mut App, event: Event) -> Result<()> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return Ok(());
    }
    if app.settings.is_some() {
        handle_settings_key(app, key.code, key.modifiers);
        return Ok(());
    }
    if app.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => resolve_approval(app, true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => resolve_approval(app, false),
            _ => {}
        }
        return Ok(());
    }
    match key.code {
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) && !app.busy => {
            app.settings = Some(SettingsState {
                provider: app.config.provider.clone(),
                api_key: String::new(),
                has_existing_key: app
                    .active_secret
                    .as_ref()
                    .is_some_and(|(preset, _)| *preset == app.config.provider.preset),
                field: SettingsField::Preset,
            });
            app.status = "Provider settings".into();
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
            if let Some(task) = app.active_task.take() {
                task.abort();
                app.busy = false;
                app.status = "Cancelled".into();
                app.entries.push(DisplayEntry {
                    kind: DisplayKind::System,
                    content: DisplayContent::Markdown("Request cancelled.".into()),
                });
            }
        }
        KeyCode::Enter if !app.busy => submit_input(app)?,
        KeyCode::Backspace if !app.busy => {
            app.input.pop();
        }
        KeyCode::Char(character)
            if !app.busy
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.input.push(character);
        }
        _ => {}
    }
    Ok(())
}

fn submit_input(app: &mut App) -> Result<()> {
    let input = app.input.trim().to_owned();
    if input.is_empty() {
        return Ok(());
    }
    let Some(runner) = app.runner.clone() else {
        app.status = "Open Provider Settings to configure an API key".into();
        return Ok(());
    };
    app.input.clear();
    app.entries.push(DisplayEntry {
        kind: DisplayKind::User,
        content: DisplayContent::Markdown(input.clone()),
    });
    app.entries.push(DisplayEntry {
        kind: DisplayKind::Assistant,
        content: DisplayContent::Markdown(String::new()),
    });
    app.conversation.push(ConversationItem::Message {
        role: Role::User,
        content: input.clone(),
    });
    app.storage
        .append_message(&app.session_id, Role::User, &input)?;
    refresh_sessions(app)?;
    app.busy = true;
    app.status = "Thinking... | Esc cancels".into();
    let items = app.conversation.clone();
    let events = app.agent_tx.clone();
    app.active_task = Some(tokio::spawn(async move {
        runner.run(items, events).await;
    }));
    trim_entries(&mut app.entries);
    Ok(())
}

fn handle_settings_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            app.settings = None;
            app.status = "Settings cancelled".into();
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
                app.status = format!("Settings error: {}", secrets::redact(&error.to_string()));
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

    let key_warning = if !entered_key.is_empty() {
        secrets::store_api_key(provider_config.preset, entered_key)
            .err()
            .map(|_| "API key is session-only")
    } else {
        None
    };
    let config_warning = app.config.save().err().map(|_| "config is session-only");
    let warnings = [key_warning, config_warning]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    app.settings = None;
    app.status = format!(
        "Ready | {} | {}{}",
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
        AgentEvent::TextDelta(delta) => {
            if let Some(entry) = app
                .entries
                .iter_mut()
                .rev()
                .find(|entry| matches!(entry.kind, DisplayKind::Assistant))
            {
                if let DisplayContent::Markdown(text) = &mut entry.content {
                    text.push_str(&delta);
                }
            }
            app.status = "Streaming... | Esc cancels".into();
        }
        AgentEvent::Approval {
            call,
            reason,
            reply,
        } => {
            app.status = "Approval required".into();
            app.pending_approval = Some(PendingApproval {
                call,
                reason,
                reply,
            });
        }
        AgentEvent::ToolStarted(call) => {
            app.status = format!("Running {}...", call.name);
            app.entries.push(DisplayEntry {
                kind: DisplayKind::Tool,
                content: DisplayContent::ToolCall {
                    name: call.name,
                    arguments: call.arguments,
                },
            });
        }
        AgentEvent::ToolFinished { call, result } => {
            app.entries.push(DisplayEntry {
                kind: DisplayKind::Tool,
                content: DisplayContent::ToolResult {
                    name: call.name,
                    result,
                },
            });
            app.status = "Returning tool result to model...".into();
        }
        AgentEvent::Usage(usage) => app.usage = usage,
        AgentEvent::Completed { items } => {
            app.conversation = items;
            app.busy = false;
            app.active_task = None;
            app.status = if refresh_sessions(app).is_ok() {
                "Ready".into()
            } else {
                "Ready | failed to refresh sessions".into()
            };
        }
        AgentEvent::Failed(error) => {
            app.entries.push(DisplayEntry {
                kind: DisplayKind::Error,
                content: DisplayContent::Markdown(secrets::redact(&error)),
            });
            app.busy = false;
            app.active_task = None;
            app.status = "Request failed".into();
        }
    }
    trim_entries(&mut app.entries);
}

fn resolve_approval(app: &mut App, approved: bool) {
    if let Some(approval) = app.pending_approval.take() {
        let _ = approval.reply.send(approved);
        app.status = if approved {
            "Approved; starting tool...".into()
        } else {
            "Rejected; returning result to model...".into()
        };
    }
}

fn create_session(app: &mut App) -> Result<()> {
    let session_id = app.storage.create_session(&app.workspace)?;
    activate_session(app, session_id)?;
    refresh_sessions(app)?;
    app.status = "New session ready".into();
    Ok(())
}

fn switch_session(app: &mut App, direction: i32) -> Result<()> {
    refresh_sessions(app)?;
    if app.sessions.len() < 2 {
        app.status = "Only one session | Ctrl+N creates another".into();
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
    let conversation = app.storage.load_messages(&session_id)?;
    let entries = display_entries(&conversation);
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
    app.usage = Usage::default();
    app.runner = runner;
    app.status = if app.runner.is_some() {
        format!(
            "Ready | {} | {}",
            app.config.provider.preset.label(),
            app.config.provider.model
        )
    } else {
        "Provider configuration required".into()
    };
    Ok(())
}

fn refresh_sessions(app: &mut App) -> Result<()> {
    app.sessions = app.storage.list_sessions(&app.workspace)?;
    Ok(())
}

fn display_entries(conversation: &[ConversationItem]) -> Vec<DisplayEntry> {
    let mut entries = conversation
        .iter()
        .filter_map(|item| match item {
            ConversationItem::Message { role, content } => Some(DisplayEntry {
                kind: match role {
                    Role::User => DisplayKind::User,
                    Role::Assistant => DisplayKind::Assistant,
                    Role::System => DisplayKind::System,
                },
                content: DisplayContent::Markdown(content.clone()),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        entries.push(DisplayEntry {
            kind: DisplayKind::System,
            content: DisplayContent::Markdown(
                "1H-Agent ready. Type a task and press Enter.".into(),
            ),
        });
    }
    entries
}

fn trim_entries(entries: &mut Vec<DisplayEntry>) {
    const MAX_ENTRIES: usize = 1000;
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
}
