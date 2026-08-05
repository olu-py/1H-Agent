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
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    agent::{AgentEvent, AgentRunner},
    config::Config,
    provider::{ConversationItem, OpenAiClient, Role, ToolCall, Usage},
    secrets,
    security::Workspace,
    storage::Storage,
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
    pub text: String,
}

pub struct PendingApproval {
    pub call: ToolCall,
    pub reason: String,
    pub reply: oneshot::Sender<bool>,
}

pub struct App {
    pub workspace: PathBuf,
    pub input: String,
    pub entries: Vec<DisplayEntry>,
    pub status: String,
    pub busy: bool,
    pub usage: Usage,
    pub pending_approval: Option<PendingApproval>,
    pub session_id: String,
    conversation: Vec<ConversationItem>,
    storage: Storage,
    runner: Option<AgentRunner>,
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
    let conversation = storage.load_messages(&session_id)?;
    let workspace = Workspace::new(&workspace_path)?;
    let registry = Arc::new(ToolRegistry::new(
        workspace,
        config.runtime.clone(),
        config.security.allow_private_networks,
    ));
    let (agent_tx, agent_rx) = mpsc::channel(128);
    let (runner, initial_status) = match secrets::openai_api_key() {
        Ok(api_key) => {
            let provider = OpenAiClient::new(config.provider.base_url.clone(), api_key)?;
            (
                Some(AgentRunner::new(
                    provider,
                    config.provider.clone(),
                    config.runtime.clone(),
                    registry,
                    storage.clone(),
                    session_id.clone(),
                )),
                format!("Ready | {}", config.provider.model),
            )
        }
        Err(error) => (None, format!("Configuration required: {error}")),
    };
    let mut entries = conversation
        .iter()
        .filter_map(|item| match item {
            ConversationItem::Message { role, content } => Some(DisplayEntry {
                kind: match role {
                    Role::User => DisplayKind::User,
                    Role::Assistant => DisplayKind::Assistant,
                    Role::System => DisplayKind::System,
                },
                text: content.clone(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        entries.push(DisplayEntry {
            kind: DisplayKind::System,
            text: "1H-Agent ready. Type a task and press Enter.".into(),
        });
    }
    let mut app = App {
        workspace: workspace_path,
        input: String::new(),
        entries,
        status: initial_status,
        busy: false,
        usage: Usage::default(),
        pending_approval: None,
        session_id,
        conversation,
        storage,
        runner,
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
    if app.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => resolve_approval(app, true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => resolve_approval(app, false),
            _ => {}
        }
        return Ok(());
    }
    match key.code {
        KeyCode::Esc => {
            if let Some(task) = app.active_task.take() {
                task.abort();
                app.busy = false;
                app.status = "Cancelled".into();
                app.entries.push(DisplayEntry {
                    kind: DisplayKind::System,
                    text: "Request cancelled.".into(),
                });
            }
        }
        KeyCode::Enter if !app.busy => submit_input(app)?,
        KeyCode::Backspace if !app.busy => {
            app.input.pop();
        }
        KeyCode::Char(character) if !app.busy && !key.modifiers.contains(KeyModifiers::CONTROL) => {
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
        app.status = "Set OPENAI_API_KEY and restart 1H-Agent".into();
        return Ok(());
    };
    app.input.clear();
    app.entries.push(DisplayEntry {
        kind: DisplayKind::User,
        text: input.clone(),
    });
    app.entries.push(DisplayEntry {
        kind: DisplayKind::Assistant,
        text: String::new(),
    });
    app.conversation.push(ConversationItem::Message {
        role: Role::User,
        content: input.clone(),
    });
    app.storage
        .append_message(&app.session_id, Role::User, &input)?;
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

fn handle_agent_event(app: &mut App, event: AgentEvent) {
    match event {
        AgentEvent::TextDelta(delta) => {
            if let Some(entry) = app
                .entries
                .iter_mut()
                .rev()
                .find(|entry| matches!(entry.kind, DisplayKind::Assistant))
            {
                entry.text.push_str(&delta);
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
                text: format!("{} {}", call.name, call.arguments),
            });
        }
        AgentEvent::ToolFinished { call, result } => {
            app.entries.push(DisplayEntry {
                kind: DisplayKind::Tool,
                text: format!("{} result:\n{}", call.name, result),
            });
            app.status = "Returning tool result to model...".into();
        }
        AgentEvent::Usage(usage) => app.usage = usage,
        AgentEvent::Completed { items } => {
            app.conversation = items;
            app.busy = false;
            app.active_task = None;
            app.status = "Ready".into();
        }
        AgentEvent::Failed(error) => {
            app.entries.push(DisplayEntry {
                kind: DisplayKind::Error,
                text: secrets::redact(&error),
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

fn trim_entries(entries: &mut Vec<DisplayEntry>) {
    const MAX_ENTRIES: usize = 1000;
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
}
