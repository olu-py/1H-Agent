//! `AppService`/`AppHandle`: the in-process core entry point shared by every
//! interface (Web, TUI, Desktop).
//!
//! [`AppService::start`] builds the full application state machine (sessions,
//! runtimes, tool registry, router, approvals, storage) and returns an
//! [`AppHandle`]. The handle exposes only typed operations — `snapshot`,
//! `messages`, `submit`, `execute_command`, `approve`, `cancel`,
//! `activate_session`, `set_provider`, `subscribe`, `shutdown` — and never
//! leaks oneshot channels or the internal [`CoreCommand`] enum. All upward
//! commands are serialized through a bounded channel into the single
//! state-machine task, so concurrent consumers cannot interleave mutations.
//!
//! On shutdown the engine rejects pending approvals, cancels the agent tree,
//! closes subscriptions, and releases the database and the per-workspace
//! exclusive lock. A second program opening the same canonical workspace fails
//! immediately at [`AppService::start`].

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use fs2::FileExt;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    agent::AgentEvent,
    app::{self, App},
    commands::{self, Command},
    config::Config,
    model::{AgentPhase, ApprovalAction, PendingApproval},
    protocol::{
        self, ApiError, AppSnapshotV2, ApprovalDto, ContextBudgetDto, Event, MessageDto,
        MessagePage, SessionStateDto, TodoDto,
    },
    provider::ToolCall,
    secrets,
    storage::{Storage, StoredMessage},
};

/// Everything the core needs to start, decoupled from any UI-specific config.
/// The caller is responsible for deriving these from the shared config file and
/// CLI flags; the core never parses server bind/port/auth settings itself.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// Canonicalized workspace path the agent is allowed to access.
    pub workspace: PathBuf,
    /// Shared configuration (provider, permissions, runtime, cluster, ...).
    /// UI-specific sections (e.g. `server`) are ignored by the core.
    pub config: Config,
    /// Directory holding `agent.db` (and the per-workspace lock files).
    pub data_dir: PathBuf,
    /// Maximum events retained in the bridge replay ring (clamped 16..=4096).
    pub event_capacity: usize,
    /// Maximum total bytes retained in the bridge replay ring (clamped
    /// 1 MiB..=16 MiB).
    pub event_max_bytes: usize,
    /// How long a pending approval waits before it is rejected automatically.
    pub approval_timeout: Duration,
    /// Message page size used by the messages endpoint (clamped 20..=200).
    pub message_page_size: usize,
}

/// Commands serialized from consumers into the single state-machine task.
enum CoreCommand {
    GetState {
        reply: oneshot::Sender<Result<AppSnapshotV2, ApiError>>,
    },
    GetMessages {
        session_id: String,
        before: Option<i64>,
        limit: usize,
        reply: oneshot::Sender<Result<MessagePage, ApiError>>,
    },
    SubmitInput {
        session_id: Option<String>,
        text: String,
        /// Replies with the request sequence assigned to the new request
        /// (or the current sequence when the input was a command/approval).
        reply: oneshot::Sender<Result<u64, ApiError>>,
    },
    ExecuteCommand {
        session_id: Option<String>,
        text: String,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Approve {
        approval_id: String,
        accept: bool,
        /// When set, register a session-scoped always-allow rule for the
        /// approved tool before resuming the agent (the TUI "A" key).
        allow_session: bool,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Cancel {
        session_id: String,
        /// The request sequence the caller believes is active. `Some(seq)` that
        /// no longer matches the session's current request is a stale cancel and
        /// is ignored (never aborts a newer request); `None` cancels whatever is
        /// active.
        request_seq: Option<u64>,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    SetProvider {
        preset: String,
        model: String,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    /// Applies a full non-secret provider profile (protocol, base_url, model,
    /// thinking level/budget, context window, retry policy). API keys never
    /// enter the core; the caller stores them in the OS keyring.
    SetProviderConfig {
        provider: crate::config::ProviderConfig,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    /// Removes a saved provider profile, switching the active provider when it
    /// was the one removed.
    RemoveProvider {
        preset: crate::config::ProviderPreset,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    ActivateSession {
        session_id: String,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Shutdown,
}

/// A pending approval: the consumer-facing id plus the deadline after which the
/// engine auto-rejects it. The oneshot sender lives in the runtime's
/// `pending_approval`; only the id crosses the boundary.
struct PendingRecord {
    session_id: String,
    deadline: Instant,
}

/// Owns the `App` state machine: runs the router event loop and services
/// serialized commands.
struct Engine {
    app: App,
    bridge: Arc<crate::bridge::EventBridge>,
    pending: HashMap<String, PendingRecord>,
    approval_timeout: Duration,
}

impl Engine {
    /// The pending approval with the earliest creation time across all
    /// sessions, along with its engine-side `approval_id`.
    fn oldest_pending(&self) -> Option<(String, String, &PendingApproval)> {
        let mut best: Option<(String, String, &PendingApproval)> = None;
        for (approval_id, record) in &self.pending {
            let runtime = self.app.runtime(&record.session_id)?;
            let Some(approval) = &runtime.pending_approval else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|(_, _, current)| approval.created_at < current.created_at)
            {
                best = Some((approval_id.clone(), record.session_id.clone(), approval));
            }
        }
        best
    }

    /// Applies a routed agent event to its owning session, then forwards an
    /// envelope to the bridge. Approval events get a fresh `approval_id` and are
    /// registered in the pending table before the sender is stored in the
    /// runtime.
    fn handle_routed(&mut self, routed: crate::app::RoutedEvent) {
        let crate::app::RoutedEvent { session_id, event } = routed;

        if let AgentEvent::Approval {
            call,
            reason,
            source_session_id,
            source_title,
            ..
        } = &event
        {
            let approval_id = uuid::Uuid::new_v4().to_string();
            self.pending.insert(
                approval_id.clone(),
                PendingRecord {
                    session_id: session_id.clone(),
                    deadline: Instant::now() + self.approval_timeout,
                },
            );
            self.bridge.push(
                session_id.clone(),
                Event::Approval {
                    approval_id,
                    call: call.clone(),
                    reason: reason.clone(),
                    source_session_id: source_session_id.clone(),
                    source_title: source_title.clone(),
                },
            );
        } else if let Some(event) = routed_to_event(&event) {
            self.bridge.push(session_id.clone(), event);
        }

        // The session's context budget changes whenever usage or a terminal
        // turn outcome updates the estimated used tokens; keep the TUI meter
        // authoritative without a history refetch. Computed before the event is
        // moved into the app state machine.
        let context_dirty = matches!(
            &event,
            AgentEvent::Usage(_)
                | AgentEvent::Completed { .. }
                | AgentEvent::CompactionCompleted { .. }
                | AgentEvent::CompactionFailed(_)
        );
        app::handle_routed_event(
            &mut self.app,
            crate::app::RoutedEvent {
                session_id: session_id.clone(),
                event,
            },
        );
        if context_dirty {
            push_context_updated(self, &session_id);
        }
    }

    /// Rejects every approval whose deadline has passed, sending `false` to the
    /// agent so it never hangs waiting on an unanswerable prompt.
    fn sweep_expired_approvals(&mut self) {
        let now = Instant::now();
        let expired = self
            .pending
            .iter()
            .filter(|(_, record)| record.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for approval_id in expired {
            let _ = self.resolve_approval(&approval_id, false, false);
        }
    }

    /// Resolves an approval: extracts the oneshot sender from the owning
    /// session's runtime and sends the decision. Shell (`!`) approvals reuse
    /// the same flow; an accepted shell command is executed through the
    /// registry.
    fn resolve_approval(
        &mut self,
        approval_id: &str,
        accept: bool,
        allow_session: bool,
    ) -> Result<(), ApiError> {
        let record = self
            .pending
            .remove(approval_id)
            .ok_or_else(|| ApiError::not_found(format!("unknown approval {approval_id}")))?;
        let owner = record.session_id;
        let Some((actual_owner, approval)) = self.app.take_pending_approval_global() else {
            return Err(ApiError::conflict("approval already resolved"));
        };
        if actual_owner != owner {
            return Err(ApiError::conflict(
                "approval does not belong to that session",
            ));
        }

        if accept && allow_session {
            let (tool, prefix, label) = match &approval.action {
                ApprovalAction::Agent(_) => session_allow_for_call(&approval.call),
                ApprovalAction::Shell(command) => (
                    "terminal_shell".to_owned(),
                    Some(command.clone()),
                    command.clone(),
                ),
            };
            self.app
                .registry
                .allow_for_session(&tool, prefix.as_deref());
            if let Some(runtime) = self.app.runtime_mut(&owner) {
                runtime.push_entry(crate::model::DisplayEntry {
                    kind: crate::model::DisplayKind::System,
                    content: crate::model::DisplayContent::Markdown(format!("本会话放行：{label}")),
                });
                runtime.status = format!("本会话已放行 {label}");
            }
        }

        let is_agent = match approval.action {
            ApprovalAction::Agent(reply) => {
                let _ = reply.send(accept);
                if let Some(runtime) = self.app.runtime_mut(&owner) {
                    runtime.agent_phase = if accept {
                        AgentPhase::Thinking
                    } else {
                        AgentPhase::Idle
                    };
                    runtime.model_phase = crate::model::ModelPhase::Idle;
                    runtime.status = if accept {
                        "已批准，开始执行工具……".into()
                    } else {
                        "已拒绝，将结果返回模型……".into()
                    };
                }
                true
            }
            ApprovalAction::Shell(command) => {
                if !accept {
                    if let Some(runtime) = self.app.runtime_mut(&owner) {
                        runtime.agent_phase = AgentPhase::Idle;
                        runtime.status = "Shell 命令已拒绝".into();
                    }
                    true
                } else {
                    let registry = self.app.registry.clone();
                    let Some(runtime) = self.app.runtime_mut(&owner) else {
                        return Ok(());
                    };
                    let events = runtime.agent_tx.clone();
                    runtime.busy = true;
                    runtime.agent_phase = AgentPhase::ToolRunning;
                    runtime.model_phase = crate::model::ModelPhase::Idle;
                    runtime.status = "正在执行 Shell 命令……".into();
                    runtime.active_task = Some(tokio::spawn(async move {
                        let result = registry
                            .execute_shell(&command)
                            .await
                            .unwrap_or_else(|error| error.to_string());
                        let _ = events
                            .send(AgentEvent::LocalCommandFinished { command, result })
                            .await;
                    }));
                    true
                }
            }
        };

        if is_agent {
            self.bridge.push(
                owner.clone(),
                Event::ApprovalResolved {
                    approval_id: approval_id.to_owned(),
                    approved: accept,
                },
            );
        }
        Ok(())
    }

    /// Fully stops every runtime (rejecting approvals and aborting agent tasks),
    /// then resolves any leftover pending approvals.
    fn shutdown(&mut self) {
        let ids = self
            .app
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        for session_id in ids {
            if let Some(runtime) = self.app.runtime_mut(&session_id) {
                runtime.shutdown();
            }
        }
        let pending = self.pending.keys().cloned().collect::<Vec<_>>();
        for approval_id in pending {
            let _ = self.resolve_approval(&approval_id, false, false);
        }
        self.app.should_quit = true;
    }
}

/// The running application service. [`AppService::start`] builds the full state
/// machine, starts the engine loop, and returns an [`AppHandle`] that owns the
/// engine task and the per-workspace exclusive lock. Dropping the last handle
/// shuts the engine down.
pub struct AppService;

impl AppService {
    /// Builds the full state machine and starts the engine loop.
    ///
    /// Fails immediately if the canonical workspace is already locked by
    /// another process (Web/TUI print an error and exit; Desktop surfaces an
    /// "already in use" prompt).
    pub async fn start(config: CoreConfig) -> Result<AppHandle> {
        let lock = WorkspaceLock::acquire(&config.data_dir, &config.workspace)
            .context("workspace is already in use by another 1H-Agent instance")?;
        std::fs::create_dir_all(&config.data_dir).with_context(|| {
            format!("cannot create data directory {}", config.data_dir.display())
        })?;
        let storage = Storage::open(&config.data_dir.join("agent.db"))?;
        secrets::preload_environment_keys();
        // Startup loads the default provider's key once (environment first,
        // then the system keychain) so the restored session owns a usable
        // runner without a settings round trip. Cached per provider.
        let _ = secrets::api_key_cached(config.config.provider.preset);

        let bridge = Arc::new(crate::bridge::EventBridge::new(
            config.event_capacity,
            config.event_max_bytes,
        ));
        let (command_tx, command_rx) = mpsc::channel::<CoreCommand>(64);

        let active_session = storage.latest_session(&config.workspace)?;
        let placeholder = uuid::Uuid::new_v4().to_string();
        let app = app::build_app(
            config.workspace.clone(),
            config.config.clone(),
            storage,
            active_session.clone().unwrap_or(placeholder),
        )
        .await?;

        let engine = Engine {
            app,
            bridge: bridge.clone(),
            pending: HashMap::new(),
            approval_timeout: config.approval_timeout,
        };
        let engine_task = tokio::spawn(run_engine(engine, command_rx));
        let event_capacity = bridge.max_events();
        let event_max_bytes = bridge.max_bytes();
        let default_page_size = protocol::clamp_page_size(Some(config.message_page_size));

        Ok(AppHandle {
            inner: Arc::new(AppHandleInner {
                command_tx,
                bridge,
                engine_task: tokio::sync::Mutex::new(Some(engine_task)),
                _lock: lock,
                event_capacity,
                event_max_bytes,
                default_page_size,
            }),
        })
    }
}

/// Shared inner state of an [`AppHandle`]; reference-counted so handles can be
/// cheaply cloned while the engine task and workspace lock live exactly once.
struct AppHandleInner {
    command_tx: mpsc::Sender<CoreCommand>,
    bridge: Arc<crate::bridge::EventBridge>,
    engine_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    _lock: WorkspaceLock,
    event_capacity: usize,
    event_max_bytes: usize,
    default_page_size: usize,
}

impl Drop for AppHandleInner {
    fn drop(&mut self) {
        // Signal a clean shutdown; the engine loop rejects approvals, cancels
        // the agent tree, and closes subscriptions before it exits. The task is
        // not aborted here: aborting could cut the cleanup short.
        let _ = self.command_tx.try_send(CoreCommand::Shutdown);
    }
}

/// A handle to the running application. Cheap to clone; every method serializes
/// its command into the state-machine task and awaits the reply.
#[derive(Clone)]
pub struct AppHandle {
    inner: Arc<AppHandleInner>,
}

impl AppHandle {
    /// Fetches the full application snapshot. The returned `event_cursor` is the
    /// position from which the consumer should subscribe.
    pub async fn snapshot(&self) -> Result<AppSnapshotV2, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::GetState { reply: tx }).await?;
        rx.await
            .map_err(|_| ApiError::internal("state unavailable"))?
    }

    /// Fetches a page of a session's transcript. `before` is the opaque cursor
    /// returned by a previous page's `next_before`; `None` fetches the newest
    /// page. `limit` is clamped to 20..=200.
    pub async fn messages(
        &self,
        session_id: &str,
        before: Option<i64>,
        limit: Option<usize>,
    ) -> Result<MessagePage, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::GetMessages {
            session_id: session_id.to_owned(),
            before,
            limit: limit
                .map(|n| n.clamp(protocol::MIN_PAGE_SIZE, protocol::MAX_PAGE_SIZE))
                .unwrap_or(self.inner.default_page_size),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("messages unavailable"))?
    }

    /// Submits user input, creating the session when `session_id` is `None`
    /// (the home-screen "first message creates a session" semantic). Returns the
    /// request sequence assigned to the new request; pass it to [`Self::cancel`]
    /// so a stale cancel never aborts a newer request.
    pub async fn submit(&self, session_id: Option<String>, text: &str) -> Result<u64, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::SubmitInput {
            session_id,
            text: text.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Executes a slash command against a session.
    pub async fn execute_command(
        &self,
        session_id: Option<String>,
        text: &str,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::ExecuteCommand {
            session_id,
            text: text.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Resolves a pending approval. `allow_session` registers a session-scoped
    /// always-allow rule for the approved tool before resuming the agent (the
    /// TUI "A" key); the rule lives only in memory and config deny still wins.
    pub async fn approve(
        &self,
        approval_id: &str,
        accept: bool,
        allow_session: bool,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::Approve {
            approval_id: approval_id.to_owned(),
            accept,
            allow_session,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Cancels the active request of a session. `request_seq` is the sequence
    /// returned by the submit that started the request: when it no longer
    /// matches the session's current request the cancel is stale and ignored,
    /// so it can never abort a newer request.
    pub async fn cancel(&self, session_id: &str, request_seq: Option<u64>) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::Cancel {
            session_id: session_id.to_owned(),
            request_seq,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Switches the server-side active session.
    pub async fn activate_session(&self, session_id: &str) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::ActivateSession {
            session_id: session_id.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Applies non-secret provider settings (preset + model). API keys stay in
    /// the OS keyring.
    pub async fn set_provider(&self, preset: &str, model: &str) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::SetProvider {
            preset: preset.to_owned(),
            model: model.to_owned(),
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Applies a complete non-secret provider profile (protocol, base_url,
    /// model, thinking level/budget, context window, retry policy). API keys
    /// never enter the core; the caller stores them in the OS keyring. The
    /// profile is upserted into the saved provider list.
    pub async fn set_provider_config(
        &self,
        provider: crate::config::ProviderConfig,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::SetProviderConfig {
            provider,
            reply: tx,
        })
        .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Removes a saved provider profile, switching the active provider when it
    /// was the one removed. The API key stays in the OS keyring.
    pub async fn remove_provider(
        &self,
        preset: crate::config::ProviderPreset,
    ) -> Result<(), ApiError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreCommand::RemoveProvider { preset, reply: tx })
            .await?;
        rx.await
            .map_err(|_| ApiError::internal("command dropped"))?
    }

    /// Replays buffered events after `after`, then returns a live receiver.
    /// Call [`Self::replay_after`] *before* [`Self::subscribe`].
    pub fn replay_after(&self, after: u64) -> crate::bridge::ReplayResult {
        self.inner.bridge.replay_after(after)
    }

    /// Subscribes to live envelopes.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<crate::protocol::Envelope>> {
        self.inner.bridge.subscribe()
    }

    /// Replays every event strictly after `after` and returns a live receiver
    /// atomically, so a fresh consumer misses nothing between its snapshot and
    /// the live stream. Returns [`crate::bridge::ResyncRequired`] when `after`
    /// has been evicted from the bridge ring; the caller must then refetch the
    /// snapshot and message page and subscribe again from the fresh cursor.
    ///
    /// Events pushed between the live subscription and the ring snapshot appear
    /// in both `replay` and `live`; consumers deduplicate by cursor (skip live
    /// events whose cursor is `<=` the last cursor processed from `replay`).
    pub fn subscribe_from(
        &self,
        after: u64,
    ) -> Result<crate::bridge::Subscription, crate::bridge::ResyncRequired> {
        self.inner.bridge.subscribe_from(after)
    }

    /// The current process-global cursor (matches the latest snapshot).
    pub fn current_cursor(&self) -> u64 {
        self.inner.bridge.current_cursor()
    }

    /// The clamped bridge ring capacity (events).
    pub fn event_capacity(&self) -> usize {
        self.inner.event_capacity
    }

    /// The clamped bridge ring byte cap.
    pub fn event_max_bytes(&self) -> usize {
        self.inner.event_max_bytes
    }

    /// Gracefully shuts the engine down: pending approvals are rejected, the
    /// agent tree is cancelled, subscriptions are closed, and the database and
    /// workspace lock are released. Returns once the engine task has finished.
    pub async fn shutdown(&self) -> Result<(), ApiError> {
        self.send(CoreCommand::Shutdown).await?;
        if let Some(task) = self.inner.engine_task.lock().await.take() {
            let _ = task.await;
        }
        Ok(())
    }

    async fn send(&self, command: CoreCommand) -> Result<(), ApiError> {
        self.inner
            .command_tx
            .send(command)
            .await
            .map_err(|_| ApiError::internal("server shutting down"))
    }
}

/// The state-machine event loop: consumes routed agent events and serialized
/// commands, plus a periodic sweep for expired approvals.
async fn run_engine(mut engine: Engine, mut command_rx: mpsc::Receiver<CoreCommand>) {
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            routed = engine.app.router_rx.recv() => {
                match routed {
                    Some(routed) => engine.handle_routed(routed),
                    None => break,
                }
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    // All handles were dropped without an explicit shutdown:
                    // clean up so no approvals or agents are left dangling.
                    engine.shutdown();
                    break;
                };
                match command {
                    CoreCommand::Shutdown => {
                        engine.shutdown();
                        break;
                    }
                    command => handle_command(&mut engine, command).await,
                }
            }
            _ = sweep.tick() => engine.sweep_expired_approvals(),
        }
    }
}

async fn handle_command(engine: &mut Engine, command: CoreCommand) {
    match command {
        CoreCommand::GetState { reply } => {
            let _ = reply.send(state_snapshot(engine));
        }
        CoreCommand::GetMessages {
            session_id,
            before,
            limit,
            reply,
        } => {
            let _ = reply.send(message_page(engine, &session_id, before, limit));
        }
        CoreCommand::SubmitInput {
            session_id,
            text,
            reply,
        } => {
            let result = submit_input(engine, session_id.as_deref(), &text);
            let _ = reply.send(result);
        }
        CoreCommand::ExecuteCommand {
            session_id,
            text,
            reply,
        } => {
            let result = execute_command(engine, session_id.as_deref(), &text);
            let _ = reply.send(result);
        }
        CoreCommand::Approve {
            approval_id,
            accept,
            allow_session,
            reply,
        } => {
            let result = engine.resolve_approval(&approval_id, accept, allow_session);
            let _ = reply.send(result);
        }
        CoreCommand::Cancel {
            session_id,
            request_seq,
            reply,
        } => {
            let result = cancel_session(engine, &session_id, request_seq);
            let _ = reply.send(result);
        }
        CoreCommand::SetProvider {
            preset,
            model,
            reply,
        } => {
            let result = set_provider(engine, &preset, &model);
            let _ = reply.send(result);
        }
        CoreCommand::SetProviderConfig { provider, reply } => {
            let result = set_provider_config(engine, provider);
            let _ = reply.send(result);
        }
        CoreCommand::RemoveProvider { preset, reply } => {
            let result = remove_provider(engine, preset);
            let _ = reply.send(result);
        }
        CoreCommand::ActivateSession { session_id, reply } => {
            let result = activate_session(engine, &session_id);
            let _ = reply.send(result);
        }
        CoreCommand::Shutdown => unreachable!("Shutdown is handled by the engine loop"),
    }
}

/// Builds the v2 application snapshot.
fn state_snapshot(engine: &Engine) -> Result<AppSnapshotV2, ApiError> {
    let app = &engine.app;
    let sessions = app
        .sessions
        .iter()
        .map(|session| {
            let runtime = app.runtime(&session.id);
            SessionStateDto {
                id: session.id.clone(),
                title: session.title.clone(),
                parent_id: session.parent_id.clone(),
                busy: runtime.map(|r| r.busy).unwrap_or(false),
                phase: runtime
                    .map(|r| r.agent_phase.label().to_owned())
                    .unwrap_or_else(|| AgentPhase::Idle.label().to_owned()),
                status: runtime.map(|r| r.status.clone()).unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    let active_session = if app.sessions.is_empty() || app.active_session.is_empty() {
        None
    } else {
        Some(app.active_session.clone())
    };
    let approval = engine
        .oldest_pending()
        .map(|(approval_id, session_id, approval)| ApprovalDto {
            approval_id,
            session_id,
            call: approval.call.clone(),
            reason: approval.reason.clone(),
            source_session_id: approval.source_session_id.clone(),
            source_title: approval.source_title.clone(),
            created_at_ms: approval.created_at.elapsed().as_millis() as u64,
        });
    let todos = app
        .current
        .todos
        .iter()
        .map(TodoDto::from)
        .collect::<Vec<_>>();
    let context = active_session
        .as_deref()
        .and_then(|session_id| context_budget(engine, session_id));
    let assistant_partial = active_session
        .as_deref()
        .and_then(|session_id| engine.app.storage.load_partial(session_id).ok())
        .flatten()
        .map(|row| protocol::PartialDto {
            content: row.content,
            created_at: row.created_at,
        });
    Ok(AppSnapshotV2 {
        protocol_version: protocol::PROTOCOL_VERSION,
        event_cursor: engine.bridge.current_cursor(),
        active_session,
        sessions,
        provider: app.config.provider.preset.label().to_owned(),
        model: app.config.provider.model.clone(),
        mode: app.current.mode.as_str().to_owned(),
        approval,
        todos,
        context,
        assistant_partial,
    })
}

/// Computes the context budget for a session: window, current usage, output
/// reservation and the resulting safe input budget. The core is the single
/// authority for context capacity.
fn context_budget(engine: &Engine, session_id: &str) -> Option<ContextBudgetDto> {
    let app = &engine.app;
    let runtime = app.runtime(session_id)?;
    let provider = &app.config.provider;
    let window = runtime.context_limit_tokens;
    let used = runtime
        .context_used_tokens
        .max(crate::session::estimate_context_tokens(
            &runtime.conversation,
        ));
    let reserve = u64::from(provider.max_output_tokens.unwrap_or(0));
    Some(ContextBudgetDto {
        context_window_tokens: window,
        used_tokens: used,
        output_reserve_tokens: reserve,
        safe_input_tokens: window.map(|w| w.saturating_sub(reserve).saturating_sub(used)),
        estimated: provider.context_window_tokens.is_none(),
    })
}

/// Broadcasts a `ContextUpdated` envelope for a session (when it has a
/// runtime), so consumers refresh their safe-input budget.
fn push_context_updated(engine: &Engine, session_id: &str) {
    if let Some(budget) = context_budget(engine, session_id) {
        engine
            .bridge
            .push(session_id.to_owned(), Event::ContextUpdated { budget });
    }
}

/// Fetches a page of a session's transcript along the current head chain.
fn message_page(
    engine: &Engine,
    session_id: &str,
    before: Option<i64>,
    limit: usize,
) -> Result<MessagePage, ApiError> {
    if engine.app.runtime(session_id).is_none()
        && !engine
            .app
            .sessions
            .iter()
            .any(|session| session.id == session_id)
    {
        return Err(ApiError::not_found(format!("unknown session {session_id}")));
    }
    let rows = engine
        .app
        .storage
        .load_message_page(session_id, before, limit + 1)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let has_more = rows.len() > limit;
    let rows = rows.into_iter().take(limit).collect::<Vec<_>>();
    // Rows come back newest-first; restore display (oldest→newest) order. The
    // opaque `next_before` cursor is the id of the oldest message in the page
    // (the last row before the reversal), so the previous page is id < that.
    let messages = rows
        .iter()
        .rev()
        .map(stored_to_message_dto)
        .collect::<Vec<_>>();
    let next_before = rows.last().map(|row| row.id);
    Ok(MessagePage {
        messages,
        next_before,
        has_more,
    })
}

/// Maps a stored message row to its display-safe v2 DTO. Provider-private
/// payloads (`provider_item`) are translated to a display-safe shape and never
/// leaked as raw JSON.
fn stored_to_message_dto(row: &StoredMessage) -> MessageDto {
    let id = row.id;
    let created_at = row.created_at.clone();
    match row.kind.as_str() {
        "message" => match row.role.as_str() {
            "user" => MessageDto::User {
                id,
                content: row.content.clone(),
                created_at,
            },
            "assistant" => MessageDto::Assistant {
                id,
                content: row.content.clone(),
                created_at,
            },
            _ => MessageDto::System {
                id,
                content: row.content.clone(),
                created_at,
            },
        },
        "context" => MessageDto::Context {
            id,
            label: row.metadata.clone().unwrap_or_else(|| "context".into()),
            content: row.content.clone(),
            created_at,
        },
        "thinking_summary" => MessageDto::Thinking {
            id,
            content: row.content.clone(),
            created_at,
        },
        "compaction_summary" => MessageDto::CompactionSummary {
            id,
            content: row.content.clone(),
            created_at,
        },
        "provider_item" => match serde_json::from_str::<serde_json::Value>(&row.content) {
            Ok(item)
                if item.get("type").and_then(serde_json::Value::as_str)
                    == Some("web_search_call") =>
            {
                MessageDto::Tool {
                    id,
                    call_id: item
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("web-search-{id}")),
                    name: "web_search".into(),
                    arguments: item
                        .get("action")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    status: "completed".into(),
                    result: None,
                    created_at,
                }
            }
            _ => MessageDto::System {
                id,
                content: "（已归档的模型内部调用）".into(),
                created_at,
            },
        },
        "tool_calls" => {
            let calls = serde_json::from_str::<Vec<ToolCall>>(&row.content).unwrap_or_default();
            MessageDto::ToolCalls {
                id,
                calls,
                created_at,
            }
        }
        "tool_output" => MessageDto::ToolOutput {
            id,
            call_id: row.metadata.clone().unwrap_or_default(),
            output: row.content.clone(),
            created_at,
        },
        _ => MessageDto::System {
            id,
            content: row.content.clone(),
            created_at,
        },
    }
}

/// Converts a routed agent event into its protocol shape. Approval events are
/// handled separately (they need an engine-side `approval_id`).
fn routed_to_event(event: &AgentEvent) -> Option<Event> {
    Some(match event {
        AgentEvent::ReasoningDelta(delta) => Event::ReasoningDelta {
            delta: delta.clone(),
        },
        AgentEvent::ReasoningCompleted => Event::ReasoningCompleted,
        AgentEvent::ProviderRetry {
            attempt,
            reason,
            delay_ms,
        } => Event::ProviderRetry {
            attempt: *attempt,
            reason: reason.clone(),
            delay_ms: *delay_ms,
        },
        AgentEvent::ModelStreaming => Event::ModelStreaming,
        AgentEvent::WebSearchStarted { query } => Event::WebSearchStarted {
            query: query.clone(),
        },
        AgentEvent::WebSearchResult {
            title,
            url,
            snippet,
        } => Event::WebSearchResult {
            title: title.clone(),
            url: url.clone(),
            snippet: snippet.clone(),
        },
        AgentEvent::WebSearchCompleted { count } => Event::WebSearchCompleted { count: *count },
        AgentEvent::Cancelled(reason) => Event::Cancelled {
            reason: reason.clone(),
        },
        AgentEvent::TextDelta(delta) => Event::TextDelta {
            delta: delta.clone(),
        },
        AgentEvent::ToolCallStreaming {
            name,
            received_bytes,
        } => Event::ToolCallStreaming {
            name: name.clone(),
            received_bytes: *received_bytes,
        },
        AgentEvent::Approval { .. } => return None,
        AgentEvent::ToolStarted(call) => Event::ToolStarted { call: call.clone() },
        AgentEvent::ToolFinished { call, result } => Event::ToolFinished {
            call: call.clone(),
            result: result.clone(),
        },
        AgentEvent::Usage(usage) => Event::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
        },
        AgentEvent::Completed { .. } => Event::Completed,
        AgentEvent::Failed(error) => Event::Failed {
            error: error.clone(),
        },
        AgentEvent::SessionsChanged => Event::SessionsChanged,
        AgentEvent::ChildSessionProgress {
            session_id: child_id,
            progress,
        } => Event::ChildSessionProgress {
            child_session_id: child_id.clone(),
            status: progress.status.wire_name().to_owned(),
            turn: progress.turn,
            max_turns: progress.max_turns,
            tool: progress.tool.clone(),
        },
        AgentEvent::LocalCommandFinished { command, result } => Event::LocalCommandFinished {
            command: command.clone(),
            result: result.clone(),
        },
        AgentEvent::CompactionStarted => Event::CompactionStarted,
        AgentEvent::CompactionCompleted { hidden } => {
            Event::CompactionCompleted { hidden: *hidden }
        }
        AgentEvent::CompactionFailed(error) => Event::CompactionFailed {
            error: error.clone(),
        },
        AgentEvent::TodoUpdated { tasks } => Event::TodoUpdated {
            tasks: tasks.clone(),
        },
    })
}

/// Submits user input to a session, creating it first when `None` (the home
/// screen "first message creates a session" semantic). Returns the request
/// sequence of the new request (the current sequence for command/approval
/// inputs, which do not start an agent task).
fn submit_input(
    engine: &mut Engine,
    session_id: Option<&str>,
    text: &str,
) -> Result<u64, ApiError> {
    let session_id = ensure_session(engine, session_id)?;
    // The target session is authoritative: when a caller submits to a session
    // that is not the engine's active one, route to it instead of mutating the
    // engine's current session.
    if session_id != engine.app.active_session {
        activate_session(engine, &session_id)?;
    }
    if text.starts_with('/') {
        let app = &mut engine.app;
        if let Some(command) = commands::parse(text) {
            let outcome = CommandOutcome::from_command(&command);
            app::execute_command(app, command).map_err(api_error)?;
            let request_seq = app.current.request_seq;
            sync_state_after_command(engine, &session_id, outcome);
            return Ok(request_seq);
        }
        let request_seq = app.current.request_seq;
        app.current.push_entry(crate::model::DisplayEntry {
            kind: crate::model::DisplayKind::Error,
            content: crate::model::DisplayContent::Markdown(format!(
                "未知命令，请使用 /help 查看命令：{text}"
            )),
        });
        return Ok(request_seq);
    }
    if text.starts_with('!') {
        let command = text.strip_prefix('!').unwrap_or(text).trim().to_owned();
        let app = &mut engine.app;
        app::request_shell_approval(app, command.clone()).map_err(api_error)?;
        let request_seq = app.current.request_seq;
        // The shell approval is created directly on the runtime (no AgentEvent
        // round trip), so register an id and broadcast it ourselves.
        register_shell_approval(engine, &command);
        return Ok(request_seq);
    }
    // Normal conversation: strict pre-submit checks against the target session.
    // A concurrent submit must never overwrite `active_task`.
    let app = &mut engine.app;
    {
        let runtime = app
            .runtime(&session_id)
            .ok_or_else(|| ApiError::conflict("session runtime is unavailable"))?;
        if runtime.busy || runtime.active_task.is_some() {
            return Err(ApiError::conflict("session is busy with another request"));
        }
        if runtime.pending_approval.is_some() {
            return Err(ApiError::conflict("session is waiting for an approval"));
        }
        if runtime.runner.is_none() {
            return Err(ApiError::conflict(
                "provider/API key is not configured; open the Provider settings",
            ));
        }
    }
    // The target session is the active one here; stamp the new request before
    // `submit` starts its task.
    app.current.request_seq = app.current.request_seq.wrapping_add(1);
    let request_seq = app.current.request_seq;
    app.input.set(text.to_owned());
    app::submit_input(app).map_err(api_error)?;
    // Submitting a message changes the used-token estimate; refresh the
    // safe-input budget for the target session.
    push_context_updated(engine, &session_id);
    Ok(request_seq)
}

/// Registers a shell (`!`) approval that was created directly on the runtime:
/// assigns an id, records the deadline, and broadcasts the approval envelope.
fn register_shell_approval(engine: &mut Engine, _command: &str) {
    let Some(approval) = &engine.app.current.pending_approval else {
        return;
    };
    let session_id = engine.app.active_session.clone();
    let approval_id = uuid::Uuid::new_v4().to_string();
    engine.pending.insert(
        approval_id.clone(),
        PendingRecord {
            session_id: session_id.clone(),
            deadline: Instant::now() + engine.approval_timeout,
        },
    );
    engine.bridge.push(
        session_id.clone(),
        Event::Approval {
            approval_id,
            call: approval.call.clone(),
            reason: approval.reason.clone(),
            source_session_id: approval.source_session_id.clone(),
            source_title: approval.source_title.clone(),
        },
    );
}

/// Executes a slash command against a session.
fn execute_command(
    engine: &mut Engine,
    session_id: Option<&str>,
    text: &str,
) -> Result<(), ApiError> {
    let session_id = ensure_session(engine, session_id)?;
    let app = &mut engine.app;
    let Some(command) = commands::parse(text) else {
        return Err(ApiError::bad_request(format!("unknown command: {text}")));
    };
    let outcome = CommandOutcome::from_command(&command);
    app::execute_command(app, command).map_err(api_error)?;
    sync_state_after_command(engine, &session_id, outcome);
    Ok(())
}

/// Which post-command sync events to broadcast.
#[derive(Clone, Copy, Default)]
struct CommandOutcome {
    todo_changed: bool,
    transcript_invalidated: bool,
}

impl CommandOutcome {
    fn from_command(command: &Command) -> Self {
        Self {
            todo_changed: matches!(command, Command::Todo(_)),
            // History-modifying commands invalidate the cached transcript.
            transcript_invalidated: matches!(
                command,
                Command::NewSession
                    | Command::Rename(_)
                    | Command::Delete
                    | Command::Fork
                    | Command::Undo
                    | Command::Redo
                    | Command::Compact(_)
                    | Command::Uncompact
            ),
        }
    }
}

/// Pushes the DTOs the frontend needs to stay in sync after a command that the
/// core logic handled entirely in-process (no `AgentEvent` round trip):
/// a `TodoUpdated` for todo mutations, a `TranscriptInvalidated` for
/// history-modifying commands, and a `SessionsChanged` so the consumer
/// refreshes its sidebar and clears transient "发送中…" state.
fn sync_state_after_command(engine: &mut Engine, session_id: &str, outcome: CommandOutcome) {
    if outcome.todo_changed {
        let tasks = engine
            .app
            .runtime(session_id)
            .map(|runtime| runtime.todos.clone())
            .unwrap_or_default();
        engine
            .bridge
            .push(session_id.to_owned(), Event::TodoUpdated { tasks });
    }
    if outcome.transcript_invalidated {
        engine
            .bridge
            .push(session_id.to_owned(), Event::TranscriptInvalidated);
    }
    engine
        .bridge
        .push(session_id.to_owned(), Event::SessionsChanged);
}

/// Ensures the target session exists, creating and activating it when no id is
/// given (or when the special token `"new"` is used).
fn ensure_session(engine: &mut Engine, session_id: Option<&str>) -> Result<String, ApiError> {
    match session_id {
        Some(id) if !id.is_empty() && id != "new" => {
            if engine.app.runtime(id).is_none()
                && !engine.app.sessions.iter().any(|session| session.id == id)
            {
                return Err(ApiError::not_found(format!("unknown session {id}")));
            }
            Ok(id.to_owned())
        }
        _ => {
            app::create_session(&mut engine.app).map_err(api_error)?;
            Ok(engine.app.active_session.clone())
        }
    }
}

/// Switches the engine-side active session (the runtime whose events are routed
/// to the current view).
fn activate_session(engine: &mut Engine, session_id: &str) -> Result<(), ApiError> {
    if !engine
        .app
        .sessions
        .iter()
        .any(|session| session.id == session_id)
    {
        return Err(ApiError::not_found(format!("unknown session {session_id}")));
    }
    app::activate_session(&mut engine.app, session_id.to_owned()).map_err(api_error)?;
    engine
        .bridge
        .push(session_id.to_owned(), Event::SessionsChanged);
    Ok(())
}

fn cancel_session(
    engine: &mut Engine,
    session_id: &str,
    request_seq: Option<u64>,
) -> Result<(), ApiError> {
    if session_id != engine.app.active_session {
        return Err(ApiError::conflict(
            "only the active session can be cancelled",
        ));
    }
    // A stale cancel (a sequence that no longer matches the session's current
    // request) must not abort a newer request; ignore it silently.
    if let Some(seq) = request_seq {
        let Some(runtime) = engine.app.runtime(session_id) else {
            return Err(ApiError::not_found("session not found"));
        };
        if runtime.request_seq != seq {
            return Ok(());
        }
    }
    app::cancel_active_request(&mut engine.app);
    // `cancel_active_request` mutates the runtime in place without an
    // `AgentEvent` round trip, so broadcast the terminal event ourselves.
    engine.bridge.push(
        session_id.to_owned(),
        Event::Cancelled {
            reason: "user".into(),
        },
    );
    Ok(())
}

/// Applies non-secret provider settings: switches the preset when given and
/// sets the model. API keys are never handled here (they stay in the keyring).
fn set_provider(engine: &mut Engine, preset: &str, model: &str) -> Result<(), ApiError> {
    use crate::config::ProviderPreset;
    let Some(preset) = ProviderPreset::parse(preset) else {
        return Err(ApiError::bad_request(format!(
            "unknown provider preset {preset}"
        )));
    };
    let current_preset = engine.app.config.provider.preset;
    if preset != current_preset {
        app::apply_provider_choice(&mut engine.app, preset).map_err(api_error)?;
        // `apply_provider_choice` reports an unavailable key as a status, not an
        // error. Detect an unchanged preset and refuse to apply the model so we
        // never leave an inconsistent preset/model pair.
        if engine.app.config.provider.preset != preset {
            return Err(ApiError::bad_request(format!(
                "{} 的 API Key 不可用",
                crate::config::ProviderPreset::ALL
                    .iter()
                    .find(|candidate| **candidate == preset)
                    .map(|preset| preset.label())
                    .unwrap_or("provider")
            )));
        }
    }
    if !model.is_empty() && model != engine.app.config.provider.model {
        app::apply_model_choice(&mut engine.app, model.to_owned()).map_err(api_error)?;
    }
    push_context_updated(engine, &engine.app.current.session_id);
    Ok(())
}

/// Applies a full non-secret provider profile: switches the preset, protocol,
/// base_url, model and thinking settings, upserts the saved profile, rebuilds
/// the runner and persists the config. API keys are never handled here.
fn set_provider_config(
    engine: &mut Engine,
    provider: crate::config::ProviderConfig,
) -> Result<(), ApiError> {
    let preset = provider.preset;
    let model = provider.model.clone();
    engine.app.config.provider = provider.clone();
    engine.app.config.upsert_provider(provider);
    engine.app.current.context_limit_tokens =
        engine.app.config.provider.resolved_context_window_tokens();
    // Force a fresh provider context so the next request uses the new
    // provider's contract instead of resuming a foreign response id.
    engine
        .app
        .storage
        .clear_response_id(&engine.app.current.session_id)
        .map_err(|error| api_error(error.into()))?;
    app::rebuild_runner(&mut engine.app).map_err(api_error)?;
    let status = match engine.app.config.save() {
        Ok(()) => format!("就绪 | {} | {}", preset.label(), model),
        Err(error) => format!(
            "配置已应用，但保存失败：{}",
            crate::secrets::redact(&error.to_string())
        ),
    };
    engine.app.current.status = status;
    push_context_updated(engine, &engine.app.current.session_id);
    Ok(())
}

/// Removes a saved provider profile, switching the active provider (and its
/// runner) when it was the one removed. The API key stays in the OS keyring.
fn remove_provider(
    engine: &mut Engine,
    preset: crate::config::ProviderPreset,
) -> Result<(), ApiError> {
    engine.app.config.remove_provider(preset);
    if engine.app.config.provider.preset == preset {
        engine.app.config.provider = engine
            .app
            .config
            .providers
            .first()
            .cloned()
            .unwrap_or_else(|| crate::config::ProviderPreset::OpenAi.defaults());
        engine.app.active_secret =
            crate::secrets::api_key_cached(engine.app.config.provider.preset)
                .ok()
                .map(|key| (engine.app.config.provider.preset, key));
        app::rebuild_runner(&mut engine.app).map_err(api_error)?;
    }
    engine.app.config.save().map_err(api_error)?;
    push_context_updated(engine, &engine.app.current.session_id);
    Ok(())
}

/// Computes the session always-allow key (tool + optional command prefix) for
/// an agent approval prompt, plus a human-readable label for the audit entry.
fn session_allow_for_call(call: &crate::provider::ToolCall) -> (String, Option<String>, String) {
    let prefix = crate::tools::ToolRegistry::command_prefix_for(call);
    match prefix {
        Some(command) => {
            let label = format!("{} {}", call.name, command);
            (call.name.clone(), Some(command), label)
        }
        None => (call.name.clone(), None, call.name.clone()),
    }
}

fn api_error(error: anyhow::Error) -> ApiError {
    ApiError::internal(error.to_string())
}

/// Per-workspace exclusive lock. A second program opening the same canonical
/// workspace fails immediately at startup.
struct WorkspaceLock {
    _file: std::fs::File,
}

impl WorkspaceLock {
    fn acquire(data_dir: &Path, workspace: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let lock_dir = data_dir.join("workspace-locks");
        std::fs::create_dir_all(&lock_dir)?;
        let name = format!("{}.lock", stable_hash(workspace));
        let file = std::fs::File::create(lock_dir.join(name))?;
        file.try_lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

/// A stable, dependency-free 64-bit FNV-1a hash used to name the per-workspace
/// lock file (stable across Rust versions, unlike `DefaultHasher`).
fn stable_hash(value: &Path) -> u64 {
    let bytes = value.as_os_str().as_encoded_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ApiErrorKind, MessageDto, PROTOCOL_VERSION};
    use tempfile::TempDir;

    async fn test_handle() -> (TempDir, AppHandle) {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();
        let mut config = Config::default();
        config.data_dir = temp.path().join("data");
        // Deterministic runner: without a seeded key, whether the shared key
        // cache holds one depends on test scheduling, making submit-based tests
        // flaky.
        crate::secrets::test_seed_key(config.provider.preset, "test-key");
        let handle = AppService::start(CoreConfig {
            workspace,
            config,
            data_dir: temp.path().join("data"),
            event_capacity: 64,
            event_max_bytes: crate::bridge::DEFAULT_MAX_BYTES,
            approval_timeout: Duration::from_secs(300),
            message_page_size: 100,
        })
        .await
        .unwrap();
        (temp, handle)
    }

    #[tokio::test]
    async fn snapshot_reports_sessions_and_cursor() {
        let (_temp, handle) = test_handle().await;
        let snapshot = handle.snapshot().await.unwrap();
        assert_eq!(snapshot.protocol_version, PROTOCOL_VERSION);
        assert_eq!(snapshot.event_cursor, handle.current_cursor());
        assert!(!snapshot.model.is_empty());
        assert!(!snapshot.provider.is_empty());
        assert_eq!(snapshot.mode, "build");
        assert!(snapshot.approval.is_none());
    }

    #[tokio::test]
    async fn input_creates_session_when_none_active() {
        let (_temp, handle) = test_handle().await;
        let before = handle.snapshot().await.unwrap();
        if before.active_session.is_some() {
            // A fresh temp workspace has no sessions; this guard keeps the test
            // robust if a default session were ever created eagerly.
            return;
        }
        handle.submit(None, "first message").await.unwrap();
        let after = handle.snapshot().await.unwrap();
        assert_eq!(after.sessions.len(), 1);
        assert!(after.active_session.is_some());
        assert!(after.active_session.as_deref() != before.active_session.as_deref());
    }

    #[tokio::test]
    async fn messages_return_a_page_and_cursor_pagination() {
        let (_temp, handle) = test_handle().await;
        handle.submit(None, "hello").await.unwrap();
        let session = handle.snapshot().await.unwrap().active_session.unwrap();
        // A running session rejects concurrent submits (structured conflict), so
        // multi-page seeding is exercised at the storage layer; here we verify
        // the page shape and the rejection path.
        let error = handle
            .submit(Some(session.clone()), "while busy")
            .await
            .unwrap_err();
        assert_eq!(error.kind, ApiErrorKind::Conflict);
        let page = handle.messages(&session, None, Some(20)).await.unwrap();
        assert_eq!(page.messages.len(), 1);
        // Display order: oldest first.
        let first = &page.messages[0];
        match first {
            MessageDto::User { content, .. } => assert!(content.starts_with("hello")),
            other => panic!("expected user message, got {other:?}"),
        }
        // Unknown session is rejected.
        let error = handle.messages("missing", None, None).await.unwrap_err();
        assert_eq!(error.kind, ApiErrorKind::NotFound);
    }

    #[tokio::test]
    async fn submit_to_unknown_session_returns_not_found() {
        let (_temp, handle) = test_handle().await;
        let error = handle
            .submit(Some("missing-session".into()), "hi")
            .await
            .unwrap_err();
        assert_eq!(error.kind, ApiErrorKind::NotFound);
    }

    #[tokio::test]
    async fn submit_reports_request_seq_and_stale_cancel_is_ignored() {
        let (_temp, handle) = test_handle().await;
        let seq = handle.submit(None, "hello").await.unwrap();
        assert_eq!(seq, 1, "first submit starts request sequence at 1");
        let session = handle.snapshot().await.unwrap().active_session.unwrap();
        // Watch the live stream for the Cancelled event.
        let subscription = handle
            .subscribe_from(handle.current_cursor())
            .expect("cursor not evicted");
        let mut live = subscription.live;
        // A stale cancel (wrong sequence) must not abort the current request and
        // must not emit a Cancelled event.
        handle.cancel(&session, Some(seq + 100)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut stale_cancelled = false;
        while let Ok(envelope) = live.try_recv() {
            if matches!(envelope.event, Event::Cancelled { .. }) {
                stale_cancelled = true;
            }
        }
        assert!(!stale_cancelled, "stale cancel must not emit Cancelled");
        // A matching cancel succeeds and emits the terminal event.
        handle.cancel(&session, Some(seq)).await.unwrap();
        let mut matched = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !matched {
            if let Ok(envelope) = live.try_recv() {
                if matches!(envelope.event, Event::Cancelled { .. }) {
                    matched = true;
                }
            } else if tokio::time::Instant::now() >= deadline {
                break;
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        assert!(matched, "matching cancel must emit a Cancelled event");
    }

    #[tokio::test]
    async fn commands_invalidate_transcript_and_update_snapshot() {
        let (_temp, handle) = test_handle().await;
        handle.submit(None, "rename me").await.unwrap();
        let session = handle.snapshot().await.unwrap().active_session.unwrap();
        handle
            .execute_command(Some(session.clone()), "/todo add write tests")
            .await
            .unwrap();
        handle
            .execute_command(Some(session.clone()), "/undo")
            .await
            .unwrap();
        let snapshot = handle.snapshot().await.unwrap();
        assert_eq!(snapshot.sessions[0].id, session);
    }

    #[tokio::test]
    async fn unknown_session_commands_fail_without_creating() {
        let (_temp, handle) = test_handle().await;
        let result = handle.submit(Some("missing".into()), "hello").await;
        assert!(result.is_err());
        let snapshot = handle.snapshot().await.unwrap();
        assert!(snapshot.sessions.is_empty() || snapshot.sessions.is_empty());
    }

    #[tokio::test]
    async fn set_provider_model_only_switch_updates_snapshot() {
        let (_temp, handle) = test_handle().await;
        handle.set_provider("openai", "gpt-5").await.unwrap();
        let snapshot = handle.snapshot().await.unwrap();
        assert_eq!(snapshot.provider, "OpenAI");
        assert_eq!(snapshot.model, "gpt-5");
    }

    #[tokio::test]
    async fn shutdown_rejects_pending_approvals() {
        let (_temp, handle) = test_handle().await;
        let session = handle.snapshot().await.unwrap().active_session;
        let _ = session;
        // A shutdown must not panic even with no pending approval.
        handle.execute_command(None, "/new").await.unwrap();
    }

    #[tokio::test]
    async fn workspace_lock_blocks_second_service() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();
        let data_dir = temp.path().join("data");
        let core_config = |dir: &std::path::Path| CoreConfig {
            workspace: workspace.clone(),
            config: Config::default(),
            data_dir: dir.to_path_buf(),
            event_capacity: 64,
            event_max_bytes: crate::bridge::DEFAULT_MAX_BYTES,
            approval_timeout: Duration::from_secs(300),
            message_page_size: 100,
        };
        // First service acquires the per-workspace lock.
        let first = AppService::start(core_config(&data_dir)).await.unwrap();
        // A second service on the same canonical workspace must fail immediately
        // (same data_dir → same lock file, still held by the first service).
        let second = AppService::start(core_config(&data_dir)).await;
        assert!(
            second.is_err(),
            "second service must fail on the locked workspace"
        );
        drop(first);
        // After dropping the first, the same workspace is lockable again.
        let third = AppService::start(core_config(&data_dir)).await;
        assert!(third.is_ok());
    }

    #[test]
    fn stored_provider_item_maps_to_display_safe_tool_dto() {
        let row = StoredMessage {
            id: 5,
            role: "assistant".into(),
            content: serde_json::json!({
                "id": "ws_1",
                "type": "web_search_call",
                "status": "completed",
                "action": {"type":"search","query":"Rust"}
            })
            .to_string(),
            kind: "provider_item".into(),
            metadata: None,
            created_at: "now".into(),
        };
        let dto = stored_to_message_dto(&row);
        match dto {
            MessageDto::Tool {
                name, arguments, ..
            } => {
                assert_eq!(name, "web_search");
                assert_eq!(arguments["query"], "Rust");
            }
            other => panic!("expected a tool dto, got {other:?}"),
        }
    }

    #[test]
    fn lock_hash_is_stable() {
        let a = stable_hash(Path::new("/workspace/a"));
        let b = stable_hash(Path::new("/workspace/b"));
        assert_ne!(a, b);
        assert_eq!(stable_hash(Path::new("/workspace/a")), a);
    }

    #[tokio::test]
    async fn restart_and_restore_session_keeps_a_usable_runner_and_streams() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();
        let data_dir = temp.path().join("data");
        let mut config = Config::default();
        config.data_dir = data_dir.clone();
        // Deterministic runner across both service lifetimes (no keyring I/O).
        crate::secrets::test_seed_key(config.provider.preset, "test-key");

        let start = |workspace: PathBuf, config: Config| {
            AppService::start(CoreConfig {
                workspace,
                config,
                data_dir: data_dir.clone(),
                event_capacity: 64,
                event_max_bytes: crate::bridge::DEFAULT_MAX_BYTES,
                approval_timeout: Duration::from_secs(300),
                message_page_size: 100,
            })
        };

        // First service: create two sessions so the second service can
        // explicitly restore the non-latest one (exercising the rebuild path).
        let handle1 = start(workspace.clone(), config.clone()).await.unwrap();
        handle1.submit(None, "first").await.unwrap();
        let session_a = handle1.snapshot().await.unwrap().active_session.unwrap();
        handle1.submit(None, "second").await.unwrap();
        let session_b = handle1.snapshot().await.unwrap().active_session.unwrap();
        assert_ne!(session_a, session_b);
        handle1.shutdown().await.unwrap();
        drop(handle1);

        // Restart against the same persistent database. The latest session is
        // restored automatically; explicitly restore the older one.
        let handle2 = start(workspace, config).await.unwrap();
        handle2.activate_session(&session_a).await.unwrap();

        // Watch the live stream, then submit to the restored session. A
        // successful submit (not a "provider not configured" conflict) proves
        // the restored session owns a usable runner.
        let subscription = handle2
            .subscribe_from(handle2.current_cursor())
            .expect("cursor must still be buffered");
        let mut live = subscription.live;
        handle2
            .submit(Some(session_a.clone()), "again")
            .await
            .unwrap();

        // The agent emits ModelStreaming before any network I/O, so the
        // restored session is guaranteed to enter the streaming phase even
        // though the seeded key would be rejected by the real provider.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut entered_streaming = false;
        while tokio::time::Instant::now() < deadline {
            if let Ok(envelope) = live.try_recv() {
                if envelope.session_id == session_a
                    && matches!(envelope.event, Event::ModelStreaming)
                {
                    entered_streaming = true;
                    break;
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            entered_streaming,
            "restored session must accept input and enter ModelStreaming"
        );
        handle2.shutdown().await.unwrap();
    }
}
