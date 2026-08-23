use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
#[cfg(test)]
use tokio::sync::oneshot;
use tokio::sync::{Mutex, mpsc};

use crate::{
    agent::{AgentEvent, AgentRunner, ChildSessionProgress, ChildSessionStatus},
    commands::{self, AgentMode, Command, TodoCommand},
    config::{Config, ProviderPreset},
    input::InputBuffer,
    provider::{ConversationItem, OpenAiClient, Role, ToolCall, Usage},
    secrets,
    security::Workspace,
    session::{
        EventCtx, SessionRuntime, display_entries, estimate_context_tokens, trim_conversation,
    },
    storage::{SessionSummary, Storage},
    tools::ToolRegistry,
};

pub(crate) use crate::model::ThinkingResult;
pub use crate::model::{
    AgentPhase, ApprovalAction, DisplayContent, DisplayEntry, DisplayKind, ModelPhase,
    PendingApproval, ThinkingDisplay, TodoDisplay, TodoStatus, TodoTask, ToolDisplay,
    ToolDisplayStatus,
};

pub struct App {
    pub workspace: PathBuf,
    pub input: InputBuffer,
    pub context_meter_enabled: bool,
    pub sessions: Vec<SessionSummary>,
    pub child_status: HashMap<String, ChildSessionProgress>,
    pub child_batches: HashMap<String, HashSet<String>>,
    pub(crate) storage: Storage,
    pub(crate) config: Config,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) approval_lock: Arc<Mutex<()>>,
    pub(crate) active_secret: Option<(ProviderPreset, String)>,
    pub(crate) active_session: String,
    pub current: SessionRuntime,
    pub(crate) background: HashMap<String, SessionRuntime>,
    pub(crate) router_tx: mpsc::Sender<RoutedEvent>,
    pub(crate) router_rx: mpsc::Receiver<RoutedEvent>,
    pub(crate) should_quit: bool,
}

/// An agent event tagged with the session it belongs to, so a single channel
/// can route events to any background session in O(1).
pub(crate) struct RoutedEvent {
    pub(crate) session_id: String,
    pub(crate) event: AgentEvent,
}

/// Builds the application state machine (sessions, runtimes, registry, router
/// channel) without touching the terminal. Shared by the TUI event loop and
/// the WebUI server, which only replaces the `router_rx` consumer.
pub(crate) async fn build_app(
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
        sessions,
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

pub(crate) fn apply_provider_choice(app: &mut App, preset: ProviderPreset) -> Result<()> {
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

pub(crate) fn apply_model_choice(app: &mut App, model: String) -> Result<()> {
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

pub(crate) fn cancel_active_request(app: &mut App) {
    if let Some(approval) = app.current.take_pending_approval() {
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

pub(crate) fn submit_input(app: &mut App) -> Result<()> {
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

pub(crate) fn execute_command(app: &mut App, command: Command) -> Result<()> {
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
        Command::Provider => {
            // Provider configuration is handled by the WebUI settings screen
            // (POST /api/config/provider); the TUI settings panel is gone.
            app.current.status = format!(
                "当前提供商：{} · {}",
                app.config.provider.preset.label(),
                app.config.provider.model
            );
        }
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
            app.current.entries.clear();
            app.current.reset_thinking_state();
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
            }
            app.child_batches.retain(|_, children| {
                children.retain(|child_id| !deleted_ids.contains(child_id));
                !children.is_empty()
            });
            let _ = app.storage.purge_soft_deleted_snapshots();
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
            let detached = app.storage.head_turn_id(&app.current.session_id)?;
            if app.storage.undo(&app.current.session_id)? {
                app.storage.clear_response_id(&app.current.session_id)?;
                let rollback_message = if let Some(turn_id) = detached {
                    restore_snapshots(app, &turn_id, SnapshotDirection::Backward)
                } else {
                    None
                };
                reload_current_session(app)?;
                refresh_sessions(app)?;
                app.current.status = match rollback_message {
                    Some(message) => format!("已撤销上一轮；{message}"),
                    None => "已撤销上一轮".into(),
                };
            } else {
                app.current.status = "没有可撤销的内容".into();
            }
        }
        Command::Redo => {
            if app.storage.redo(&app.current.session_id)? {
                let advanced = app.storage.head_turn_id(&app.current.session_id)?;
                app.storage.clear_response_id(&app.current.session_id)?;
                let rollback_message = if let Some(turn_id) = advanced {
                    restore_snapshots(app, &turn_id, SnapshotDirection::Forward)
                } else {
                    None
                };
                reload_current_session(app)?;
                refresh_sessions(app)?;
                app.current.status = match rollback_message {
                    Some(message) => format!("已重做上一轮；{message}"),
                    None => "已重做上一轮".into(),
                };
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

pub(crate) fn handle_todo_command(app: &mut App, action: TodoCommand) -> Result<()> {
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
    app.current.set_todos(tasks);
    Ok(())
}

pub(crate) fn export_session(app: &mut App, requested: Option<String>) -> Result<()> {
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

pub(crate) fn rebuild_runner(app: &mut App) -> Result<()> {
    let Some((_, api_key)) = &app.active_secret else {
        app.current.runner = None;
        return Ok(());
    };
    let provider = OpenAiClient::new_with_retry(
        app.config.provider.base_url.clone(),
        api_key.clone(),
        app.config.provider.retry_max_attempts,
        app.config.provider.retry_initial_backoff_ms,
        app.config.provider.retry_max_backoff_ms,
    )?;
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

pub(crate) fn start_diff(app: &mut App) -> Result<()> {
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

pub(crate) fn handle_routed_event(app: &mut App, routed: RoutedEvent) -> bool {
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

/// How a pending approval prompt was answered.
pub(crate) fn request_shell_approval(app: &mut App, command: String) -> Result<()> {
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

pub(crate) fn create_session(app: &mut App) -> Result<()> {
    let session_id = app.storage.create_session(&app.workspace)?;
    activate_session(app, session_id)?;
    refresh_sessions(app)?;
    app.current.status = "新会话已就绪".into();
    Ok(())
}

/// Shared mode-switch entry point for slash commands, palette actions, and
/// clicking the mode label in the input title. It updates UI state,
/// tool permissions, persistence, and clears the provider response id so the
/// next request uses the new mode contract.
pub(crate) fn switch_mode(app: &mut App, mode: AgentMode) -> Result<()> {
    app.current.mode = mode;
    app.registry.set_mode(mode);
    let _ = app
        .storage
        .set_session_mode(&app.current.session_id, mode.as_str());
    app.storage.clear_response_id(&app.current.session_id)?;
    app.current.status = format!("模式已切换为 {}", mode.as_str().to_ascii_uppercase());
    Ok(())
}

/// Resolves the provider configuration used by a spawned child agent, falling
/// back to the session's default provider when the child has none of its own.
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
        OpenAiClient::new_with_retry(
            provider_config.base_url.clone(),
            api_key.clone(),
            provider_config.retry_max_attempts,
            provider_config.retry_initial_backoff_ms,
            provider_config.retry_max_backoff_ms,
        )
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
        thinking_result: ThinkingResult::Completed,
        usage: Usage::default(),
        context_used_tokens: estimate_context_tokens(&conversation),
        context_limit_tokens: provider_config.resolved_context_window_tokens(),
        pending_approval: None,
        mode,
        child_role,
        conversation,
        runner,
        agent_tx,
        active_task: None,
        parked_at: Instant::now(),
    }
}

pub(crate) fn reload_current_session(app: &mut App) -> Result<()> {
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
    Ok(())
}

/// Direction for `restore_snapshots`: `Backward` (undo) writes each file's
/// pre-image (deleting files that did not exist), `Forward` (redo) writes the
/// post-image.
#[derive(Clone, Copy)]
enum SnapshotDirection {
    Backward,
    Forward,
}

/// Rolls the file snapshots recorded on `turn_id` back to disk for undo or
/// forward for redo. Returns a human-readable summary of any files that could
/// not be restored, or `None` when every snapshot applied cleanly.
fn restore_snapshots(app: &mut App, turn_id: &str, direction: SnapshotDirection) -> Option<String> {
    let snapshots = app
        .storage
        .restore_turn_files(&app.current.session_id, turn_id)
        .ok()?;
    let mut problems = Vec::new();
    let mut ordered = snapshots;
    match direction {
        SnapshotDirection::Backward => ordered.reverse(),
        SnapshotDirection::Forward => {}
    }
    for snapshot in ordered {
        let relative = PathBuf::from(&snapshot.path);
        let resolved = app.workspace.join(&relative);
        let (image, existed) = match direction {
            SnapshotDirection::Backward => (
                snapshot.pre_image.as_ref(),
                snapshot.existed && snapshot.pre_image.is_some(),
            ),
            SnapshotDirection::Forward => {
                (snapshot.post_image.as_ref(), snapshot.post_image.is_some())
            }
        };
        let Some(image) = image else {
            if !snapshot.existed {
                // Marker: file exceeded the snapshot limit and was skipped.
                problems.push(format!("{} 超出快照上限，未回滚", snapshot.path));
            }
            continue;
        };
        let write_result = if existed {
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent).and_then(|_| std::fs::write(&resolved, image))
            } else {
                std::fs::write(&resolved, image)
            }
        } else {
            let _ = std::fs::remove_file(&resolved);
            Ok(())
        };
        if let Err(error) = write_result {
            problems.push(format!("{}: {error}", snapshot.path));
        }
    }
    if problems.is_empty() {
        None
    } else {
        Some(problems.join("；"))
    }
}

pub(crate) fn activate_session(app: &mut App, session_id: String) -> Result<()> {
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
    app.current.status = if app.current.runner.is_some() {
        "就绪".into()
    } else {
        "需要配置提供商".into()
    };
    Ok(())
}

pub(crate) fn evict_background_overflow(app: &mut App) {
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

pub(crate) fn refresh_sessions(app: &mut App) -> Result<()> {
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

    pub(crate) fn take_pending_approval_global(&mut self) -> Option<(String, PendingApproval)> {
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

    pub(crate) fn runtime_mut(&mut self, session_id: &str) -> Option<&mut SessionRuntime> {
        if session_id == self.active_session {
            Some(&mut self.current)
        } else {
            self.background.get_mut(session_id)
        }
    }

    /// Immutable access to a runtime by id (current or background).
    pub(crate) fn runtime(&self, session_id: &str) -> Option<&SessionRuntime> {
        if session_id == self.active_session {
            Some(&self.current)
        } else {
            self.background.get(session_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    use crate::session::{MAX_THINKING_LINE_BYTES, estimate_context_tokens};
    use std::time::Duration;

    fn handle_event_for_test(app: &mut App, event: AgentEvent) -> crate::session::SessionOutcome {
        let ctx = crate::session::EventCtx {
            storage: &app.storage,
            workspace: &app.workspace,
        };
        app.current.handle_event(&ctx, event)
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

    #[tokio::test]
    async fn undo_rolls_back_snapshotted_file_and_redo_restores_it() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let session_id = app.current.session_id.clone();
        // New head turn that undo will detach.
        app.storage
            .append_message(&session_id, Role::User, "write")
            .unwrap();
        let turn = app.storage.head_turn_id(&session_id).unwrap().unwrap();

        let file = temp.path().join("a.txt");
        std::fs::write(&file, b"after").unwrap();
        app.storage
            .snapshot_file(
                &session_id,
                &turn,
                "call_1",
                "a.txt",
                Some(b"before"),
                true,
                1024 * 1024,
                16 * 1024 * 1024,
            )
            .unwrap();
        app.storage
            .save_post_image("call_1", Some(b"after"))
            .unwrap();

        execute_command(&mut app, Command::Undo).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"before");

        execute_command(&mut app, Command::Redo).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"after");
    }

    #[tokio::test]
    async fn undo_without_snapshot_keeps_file_untouched() {
        let temp = TempDir::new().unwrap();
        let mut app = test_app(&temp);
        let session_id = app.current.session_id.clone();
        app.storage
            .append_message(&session_id, Role::User, "write")
            .unwrap();
        let file = temp.path().join("a.txt");
        std::fs::write(&file, b"content").unwrap();

        execute_command(&mut app, Command::Undo).unwrap();
        // No snapshot was recorded; the file must be left exactly as it was.
        assert_eq!(std::fs::read(&file).unwrap(), b"content");
        assert_eq!(app.current.status, "已撤销上一轮");
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
            thinking_result: ThinkingResult::Completed,
            usage: Usage::default(),
            context_used_tokens: 1,
            context_limit_tokens: None,
            pending_approval: None,
            mode: AgentMode::default(),
            child_role: None,
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
            sessions,
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
        assert_eq!(app.current.thinking_result, ThinkingResult::Completed);
        assert_eq!(thinking_summary_count(&app), 1);
        assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("正在分析工具结果")));

        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("最后失败位置".into()));
        handle_event_for_test(&mut app, AgentEvent::Failed("failed".into()));
        assert!(!app.current.thinking_active);
        assert_eq!(app.current.thinking_result, ThinkingResult::Failed);
        assert_eq!(thinking_summary_count(&app), 2);
        assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("最后失败位置")));

        handle_event_for_test(&mut app, AgentEvent::ModelStreaming);
        handle_event_for_test(&mut app, AgentEvent::ReasoningDelta("取消前内容".into()));
        handle_event_for_test(&mut app, AgentEvent::Cancelled("cancelled".into()));
        assert!(!app.current.thinking_active);
        assert_eq!(app.current.thinking_result, ThinkingResult::Cancelled);
        assert_eq!(thinking_summary_count(&app), 3);
        assert!(last_thinking_summary(&app).is_some_and(|text| text.contains("取消前内容")));
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
}
