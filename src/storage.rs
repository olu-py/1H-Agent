use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use uuid::Uuid;

use crate::provider::{ConversationItem, Role, ToolCall};

#[derive(Clone)]
pub struct Storage {
    connection: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("invalid stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("storage lock is poisoned")]
    Poisoned,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                StorageError::Database(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
            })?;
        }
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    pub fn in_memory() -> Result<Self, StorageError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StorageError> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                workspace TEXT NOT NULL,
                title TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                mode TEXT NOT NULL DEFAULT 'build',
                provider TEXT NOT NULL DEFAULT 'openai',
                model TEXT NOT NULL DEFAULT '',
                parent_id TEXT,
                deleted_at TEXT,
                head_turn_id TEXT
            );
            CREATE TABLE IF NOT EXISTS turns (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                parent_id TEXT REFERENCES turns(id) ON DELETE SET NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                turn_id TEXT,
                kind TEXT NOT NULL DEFAULT 'message',
                hidden INTEGER NOT NULL DEFAULT 0,
                metadata TEXT
            );
            CREATE TABLE IF NOT EXISTS tool_calls (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                arguments TEXT NOT NULL,
                decision TEXT NOT NULL,
                result TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT
            );
            CREATE TABLE IF NOT EXISTS provider_state (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                response_id TEXT,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS compactions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                hidden_ids TEXT NOT NULL,
                summary TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            INSERT OR IGNORE INTO schema_migrations(version, applied_at)
            VALUES (1, CURRENT_TIMESTAMP);
            INSERT OR IGNORE INTO schema_migrations(version, applied_at)
            VALUES (2, CURRENT_TIMESTAMP);
            ",
        )?;
        // These checks keep databases created by the first release compatible
        // without relying on SQLite's optional ALTER TABLE syntax extensions.
        ensure_column(
            &connection,
            "sessions",
            "mode",
            "TEXT NOT NULL DEFAULT 'build'",
        )?;
        ensure_column(
            &connection,
            "sessions",
            "provider",
            "TEXT NOT NULL DEFAULT 'openai'",
        )?;
        ensure_column(&connection, "sessions", "model", "TEXT NOT NULL DEFAULT ''")?;
        ensure_column(&connection, "sessions", "parent_id", "TEXT")?;
        ensure_column(&connection, "sessions", "deleted_at", "TEXT")?;
        ensure_column(&connection, "sessions", "head_turn_id", "TEXT")?;
        ensure_column(&connection, "sessions", "child_role", "TEXT")?;
        ensure_column(&connection, "messages", "turn_id", "TEXT")?;
        ensure_column(
            &connection,
            "messages",
            "kind",
            "TEXT NOT NULL DEFAULT 'message'",
        )?;
        ensure_column(
            &connection,
            "messages",
            "hidden",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&connection, "messages", "metadata", "TEXT")?;
        backfill_turns(&connection)?;
        connection.execute(
            "INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES (2, CURRENT_TIMESTAMP)",
            [],
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn create_session(&self, workspace: &Path) -> Result<String, StorageError> {
        let id = Uuid::new_v4().to_string();
        let turn_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO sessions(id, workspace, title, created_at, updated_at, mode, provider, model, head_turn_id) VALUES (?1, ?2, ?3, ?4, ?4, 'build', 'openai', '', ?5)",
            params![id, workspace.display().to_string(), "New session", now, turn_id],
        )?;
        connection.execute(
            "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
            params![turn_id, id, now],
        )?;
        Ok(id)
    }

    /// Creates a child session nested under `parent_id`. The child owns its own
    /// provider/model so a cluster can run different roles on different models.
    /// `mode` is the session mode used when the child is opened later, and
    /// `child_role` preserves the role-based tool restrictions for that later
    /// interaction (implement roles may write files but still never receive
    /// terminal or spawn tools).
    #[allow(clippy::too_many_arguments)]
    pub fn create_child_session(
        &self,
        workspace: &Path,
        parent_id: &str,
        provider: &str,
        model: &str,
        title: &str,
        mode: &str,
        child_role: &str,
    ) -> Result<String, StorageError> {
        let id = Uuid::new_v4().to_string();
        let turn_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO sessions(id, workspace, title, created_at, updated_at, mode, provider, model, parent_id, head_turn_id, child_role) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![id, workspace.display().to_string(), title, now, mode, provider, model, parent_id, turn_id, child_role],
        )?;
        connection.execute(
            "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
            params![turn_id, id, now],
        )?;
        Ok(id)
    }

    /// Returns the stored provider preset id and model for a session.
    pub fn session_provider_model(
        &self,
        session_id: &str,
    ) -> Result<(String, String), StorageError> {
        self.lock()?
            .query_row(
                "SELECT provider, model FROM sessions WHERE id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(StorageError::from)
    }

    /// Returns the child role captured at spawn time, if this is a child session.
    pub fn session_child_role(&self, session_id: &str) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT child_role FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    /// Returns the workspace path this session belongs to.
    pub fn session_workspace(&self, session_id: &str) -> Result<String, StorageError> {
        self.lock()?
            .query_row(
                "SELECT workspace FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
    }

    pub fn latest_session(&self, workspace: &Path) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT id FROM sessions WHERE workspace = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC LIMIT 1",
                [workspace.display().to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn list_sessions(&self, workspace: &Path) -> Result<Vec<SessionSummary>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, title, parent_id FROM sessions WHERE workspace = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC, created_at DESC",
        )?;
        let rows = statement.query_map([workspace.display().to_string()], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                parent_id: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn append_message(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let current_turn: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let turn_id = current_turn
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if current_turn.is_none() {
            let now = Utc::now().to_rfc3339();
            connection.execute(
                "INSERT OR IGNORE INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
                params![turn_id, session_id, now],
            )?;
            connection.execute(
                "UPDATE sessions SET head_turn_id = ?2 WHERE id = ?1",
                params![session_id, turn_id],
            )?;
        }
        if role == Role::User {
            let child = Uuid::new_v4().to_string();
            let now = Utc::now().to_rfc3339();
            connection.execute(
                "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![child, session_id, turn_id, now],
            )?;
            connection.execute(
                "UPDATE sessions SET head_turn_id = ?2 WHERE id = ?1",
                params![session_id, child],
            )?;
            return append_message_on_turn(&connection, session_id, &child, role, content);
        }
        append_message_on_turn(&connection, session_id, &turn_id, role, content)
    }

    pub fn append_context(
        &self,
        session_id: &str,
        label: &str,
        content: &str,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let turn_id: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(turn_id) = turn_id else {
            return Ok(());
        };
        let now = Utc::now().to_rfc3339();
        connection.execute(
            "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, metadata) VALUES (?1, 'context', ?2, ?3, ?4, 'context', 0, ?5)",
            params![session_id, content, now, turn_id, label],
        )?;
        connection.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn append_thinking_summary(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), StorageError> {
        self.append_typed_item(session_id, "thinking", "thinking_summary", content, None)
    }

    pub fn append_compaction_summary(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), StorageError> {
        self.append_typed_item(session_id, "user", "compaction_summary", content, None)
    }

    pub fn compact_with_summary(
        &self,
        session_id: &str,
        summary: &str,
        keep: usize,
    ) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        let tx = connection.unchecked_transaction()?;
        let ids = {
            let mut stmt = tx.prepare(
                "SELECT id FROM messages WHERE session_id = ?1 AND hidden = 0 ORDER BY id DESC",
            )?;
            stmt.query_map([session_id], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let hidden: Vec<i64> = ids.into_iter().skip(keep).collect();
        let now = Utc::now().to_rfc3339();
        tx.execute("INSERT INTO compactions(session_id, hidden_ids, summary, created_at) VALUES (?1, ?2, ?3, ?4)", params![session_id, serde_json::to_string(&hidden)?, summary, now])?;
        for id in &hidden {
            tx.execute("UPDATE messages SET hidden = 1 WHERE id = ?1", [id])?;
        }
        let turn: Option<String> = tx
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(turn_id) = turn {
            tx.execute("INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden) VALUES (?1, 'user', ?2, ?3, ?4, 'compaction_summary', 0)", params![session_id, summary, now, turn_id])?;
        }
        tx.execute(
            "DELETE FROM provider_state WHERE session_id = ?1",
            [session_id],
        )?;
        tx.commit()?;
        Ok(hidden.len())
    }

    pub fn restore_latest_compaction(&self, session_id: &str) -> Result<bool, StorageError> {
        let connection = self.lock()?;
        let tx = connection.unchecked_transaction()?;
        let row: Option<(i64, String)> = tx.query_row("SELECT id, hidden_ids FROM compactions WHERE session_id = ?1 ORDER BY id DESC LIMIT 1", [session_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((id, encoded)) = row else {
            return Ok(false);
        };
        let ids: Vec<i64> = serde_json::from_str(&encoded)?;
        for msg_id in ids {
            tx.execute("UPDATE messages SET hidden = 0 WHERE id = ?1", [msg_id])?;
        }
        tx.execute("UPDATE messages SET hidden = 1 WHERE session_id = ?1 AND kind = 'compaction_summary' AND id = (SELECT max(id) FROM messages WHERE session_id = ?1 AND kind = 'compaction_summary')", [session_id])?;
        tx.execute("DELETE FROM compactions WHERE id = ?1", [id])?;
        tx.execute(
            "DELETE FROM provider_state WHERE session_id = ?1",
            [session_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn append_provider_item(
        &self,
        session_id: &str,
        item: &serde_json::Value,
    ) -> Result<(), StorageError> {
        self.append_typed_item(
            session_id,
            "assistant",
            "provider_item",
            &serde_json::to_string(item)?,
            None,
        )
    }

    pub fn append_tool_calls(
        &self,
        session_id: &str,
        calls: &[ToolCall],
    ) -> Result<(), StorageError> {
        let content = serde_json::to_string(calls)?;
        self.append_typed_item(session_id, "assistant", "tool_calls", &content, None)
    }

    pub fn append_tool_output(
        &self,
        session_id: &str,
        call_id: &str,
        output: &str,
    ) -> Result<(), StorageError> {
        self.append_typed_item(session_id, "tool", "tool_output", output, Some(call_id))
    }

    fn append_typed_item(
        &self,
        session_id: &str,
        role: &str,
        kind: &str,
        content: &str,
        metadata: Option<&str>,
    ) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let turn_id: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(turn_id) = turn_id else {
            return Ok(());
        };
        let now = Utc::now().to_rfc3339();
        connection.execute(
            "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
            params![session_id, role, content, now, turn_id, kind, metadata],
        )?;
        connection.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<(), StorageError> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(());
        }
        let title = title.chars().take(120).collect::<String>();
        self.lock()?.execute(
            "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1 AND deleted_at IS NULL",
            params![session_id, title, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            "WITH RECURSIVE descendants(id) AS (
                 SELECT id FROM sessions WHERE id = ?1
                 UNION ALL
                 SELECT s.id FROM sessions s JOIN descendants d ON s.parent_id = d.id
             )
             UPDATE sessions SET deleted_at = ?2 WHERE id IN (SELECT id FROM descendants)",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn fork_session(&self, session_id: &str) -> Result<String, StorageError> {
        let connection = self.lock()?;
        let (workspace, title, mode, provider, model): (String, String, String, String, String) =
            connection.query_row(
                "SELECT workspace, title, mode, provider, model FROM sessions WHERE id = ?1",
                [session_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        let new_id = Uuid::new_v4().to_string();
        let root_turn = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        connection.execute(
            "INSERT INTO sessions(id, workspace, title, created_at, updated_at, mode, provider, model, parent_id, head_turn_id) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![new_id, workspace, format!("{title} (fork)"), now, mode, provider, model, session_id, root_turn],
        )?;
        connection.execute(
            "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
            params![root_turn, new_id, now],
        )?;
        let rows = {
            let mut statement = connection.prepare(
                "SELECT role, content, kind, hidden, metadata FROM messages WHERE session_id = ?1 ORDER BY id ASC",
            )?;
            statement
                .query_map([session_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (role, content, kind, hidden, metadata) in rows {
            connection.execute(
                "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![new_id, role, content, now, root_turn, kind, hidden, metadata],
            )?;
        }
        Ok(new_id)
    }

    pub fn undo(&self, session_id: &str) -> Result<bool, StorageError> {
        let connection = self.lock()?;
        let head: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(head) = head else { return Ok(false) };
        let parent: Option<String> = connection.query_row(
            "SELECT parent_id FROM turns WHERE id = ?1",
            [&head],
            |row| row.get(0),
        )?;
        let Some(parent) = parent else {
            return Ok(false);
        };
        connection.execute(
            "UPDATE sessions SET head_turn_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, parent, Utc::now().to_rfc3339()],
        )?;
        Ok(true)
    }

    pub fn redo(&self, session_id: &str) -> Result<bool, StorageError> {
        let connection = self.lock()?;
        let head: Option<String> = connection
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(head) = head else { return Ok(false) };
        let child: Option<String> = connection
            .query_row(
                "SELECT id FROM turns WHERE session_id = ?1 AND parent_id = ?2 ORDER BY created_at DESC LIMIT 1",
                params![session_id, head],
                |row| row.get(0),
            )
            .optional()?;
        let Some(child) = child else { return Ok(false) };
        connection.execute(
            "UPDATE sessions SET head_turn_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, child, Utc::now().to_rfc3339()],
        )?;
        Ok(true)
    }

    pub fn compact_session(&self, session_id: &str, keep: usize) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        let ids = {
            let mut statement = connection.prepare(
                "SELECT id FROM messages WHERE session_id = ?1 AND hidden = 0 ORDER BY id DESC",
            )?;
            statement
                .query_map([session_id], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let hidden = ids.into_iter().skip(keep).collect::<Vec<_>>();
        for id in &hidden {
            connection.execute("UPDATE messages SET hidden = 1 WHERE id = ?1", [id])?;
        }
        Ok(hidden.len())
    }

    pub fn set_session_mode(&self, session_id: &str, mode: &str) -> Result<(), StorageError> {
        self.lock()?.execute(
            "UPDATE sessions SET mode = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, mode, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn session_mode(&self, session_id: &str) -> Result<String, StorageError> {
        self.lock()?
            .query_row(
                "SELECT mode FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<ConversationItem>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "WITH RECURSIVE chain(id) AS (
                 SELECT head_turn_id FROM sessions WHERE id = ?1
                 UNION ALL
                 SELECT turns.parent_id FROM turns JOIN chain ON turns.id = chain.id
                 WHERE turns.parent_id IS NOT NULL
             )
             SELECT role, content, kind, metadata FROM messages
             WHERE session_id = ?1 AND hidden = 0 AND (turn_id IN (SELECT id FROM chain) OR turn_id IS NULL)
             ORDER BY id ASC",
        )?;
        let rows = statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(role, content, kind, metadata)| match kind.as_str() {
                "context" => Ok(ConversationItem::Context {
                    label: metadata.unwrap_or_else(|| "context".into()),
                    content,
                }),
                "thinking_summary" => Ok(ConversationItem::ThinkingSummary { content }),
                "compaction_summary" => Ok(ConversationItem::CompactionSummary { content }),
                "provider_item" => Ok(ConversationItem::ProviderItem {
                    item: serde_json::from_str(&content)?,
                }),
                "tool_calls" => Ok(ConversationItem::AssistantToolCalls {
                    calls: serde_json::from_str(&content)?,
                }),
                "tool_output" => Ok(ConversationItem::ToolOutput {
                    call_id: metadata.unwrap_or_default(),
                    output: content,
                }),
                _ if role == "context" => Ok(ConversationItem::Context {
                    label: metadata.unwrap_or_else(|| "context".into()),
                    content,
                }),
                _ => Ok(ConversationItem::Message {
                    role: match role.as_str() {
                        "system" => Role::System,
                        "assistant" => Role::Assistant,
                        _ => Role::User,
                    },
                    content,
                }),
            })
            .collect()
    }

    pub fn begin_tool(
        &self,
        session_id: &str,
        call: &ToolCall,
        decision: &str,
    ) -> Result<(), StorageError> {
        self.lock()?.execute(
            "INSERT OR REPLACE INTO tool_calls(id, session_id, name, arguments, decision, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                call.id,
                session_id,
                call.name,
                call.arguments.to_string(),
                decision,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn finish_tool(&self, call_id: &str, result: &str) -> Result<(), StorageError> {
        self.lock()?.execute(
            "UPDATE tool_calls SET result = ?2, finished_at = ?3 WHERE id = ?1",
            params![call_id, result, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn save_response_id(
        &self,
        session_id: &str,
        response_id: &str,
    ) -> Result<(), StorageError> {
        self.lock()?.execute(
            "INSERT INTO provider_state(session_id, response_id, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET response_id = excluded.response_id, updated_at = excluded.updated_at",
            params![session_id, response_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn response_id(&self, session_id: &str) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT response_id FROM provider_state WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn clear_response_id(&self, session_id: &str) -> Result<(), StorageError> {
        self.lock()?.execute(
            "DELETE FROM provider_state WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StorageError> {
        self.connection.lock().map_err(|_| StorageError::Poisoned)
    }
}

fn append_message_on_turn(
    connection: &Connection,
    session_id: &str,
    turn_id: &str,
    role: Role,
    content: &str,
) -> Result<(), StorageError> {
    let role_name = match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let now = Utc::now().to_rfc3339();
    connection.execute(
        "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden) VALUES (?1, ?2, ?3, ?4, ?5, 'message', 0)",
        params![session_id, role_name, content, now, turn_id],
    )?;
    connection.execute(
        "UPDATE sessions SET updated_at = ?2, title = CASE WHEN title = 'New session' AND ?3 = 'user' THEN substr(?4, 1, 80) ELSE title END WHERE id = ?1",
        params![session_id, now, role_name, content],
    )?;
    Ok(())
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), StorageError> {
    let exists = connection
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column);
    if !exists {
        connection.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

fn backfill_turns(connection: &Connection) -> Result<(), StorageError> {
    let sessions = {
        let mut statement = connection.prepare("SELECT id, head_turn_id FROM sessions")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (session_id, head) in sessions {
        let turn_id = if let Some(head) = head {
            head
        } else {
            let turn_id = Uuid::new_v4().to_string();
            connection.execute(
                "INSERT INTO turns(id, session_id, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
                params![turn_id, session_id, Utc::now().to_rfc3339()],
            )?;
            connection.execute(
                "UPDATE sessions SET head_turn_id = ?2 WHERE id = ?1",
                params![session_id, turn_id],
            )?;
            turn_id
        };
        connection.execute(
            "UPDATE messages SET turn_id = ?2 WHERE session_id = ?1 AND turn_id IS NULL",
            params![session_id, turn_id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn child_session_nests_under_parent_and_keeps_provider_model() {
        let storage = Storage::in_memory().unwrap();
        let root = tempdir().unwrap();
        let parent = storage.create_session(root.path()).unwrap();
        let child = storage
            .create_child_session(
                root.path(),
                &parent,
                "deepseek",
                "deepseek-v4-pro",
                "计划",
                "explore",
                "planner",
            )
            .unwrap();

        let sessions = storage.list_sessions(root.path()).unwrap();
        let child_summary = sessions.iter().find(|session| session.id == child).unwrap();
        assert_eq!(child_summary.parent_id.as_deref(), Some(parent.as_str()));
        assert_eq!(child_summary.title, "计划");

        let (provider, model) = storage.session_provider_model(&child).unwrap();
        assert_eq!(provider, "deepseek");
        assert_eq!(model, "deepseek-v4-pro");
        assert_eq!(storage.session_mode(&child).unwrap(), "explore");
        assert_eq!(
            storage.session_child_role(&child).unwrap().as_deref(),
            Some("planner")
        );
    }

    #[test]
    fn delete_session_soft_deletes_descendants() {
        let storage = Storage::in_memory().unwrap();
        let root = tempdir().unwrap();
        let parent = storage.create_session(root.path()).unwrap();
        let child = storage
            .create_child_session(
                root.path(),
                &parent,
                "openai",
                "gpt-5-mini",
                "child",
                "explore",
                "reviewer",
            )
            .unwrap();
        let grandchild = storage
            .create_child_session(
                root.path(),
                &child,
                "openai",
                "gpt-5-mini",
                "grandchild",
                "explore",
                "reviewer",
            )
            .unwrap();

        storage.delete_session(&parent).unwrap();
        let sessions = storage.list_sessions(root.path()).unwrap();
        assert!(sessions.is_empty());

        // Directly deleting a child leaves other branches alone.
        let parent2 = storage.create_session(root.path()).unwrap();
        let child2 = storage
            .create_child_session(
                root.path(),
                &parent2,
                "openai",
                "gpt-5-mini",
                "child2",
                "explore",
                "reviewer",
            )
            .unwrap();
        storage.delete_session(&child2).unwrap();
        let sessions = storage.list_sessions(root.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, parent2);
        let _ = grandchild;
    }

    #[test]
    fn stores_and_loads_messages_and_provider_state() {
        let storage = Storage::in_memory().unwrap();
        let root = tempdir().unwrap();
        let session = storage.create_session(root.path()).unwrap();
        storage
            .append_message(&session, Role::User, "hello")
            .unwrap();
        assert_eq!(storage.load_messages(&session).unwrap().len(), 1);
        storage.save_response_id(&session, "resp_1").unwrap();
        assert_eq!(
            storage.response_id(&session).unwrap().as_deref(),
            Some("resp_1")
        );
        assert_eq!(
            storage.latest_session(root.path()).unwrap().as_deref(),
            Some(session.as_str())
        );
        let sessions = storage.list_sessions(root.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session);
        assert_eq!(sessions[0].title, "hello");
    }

    #[test]
    fn compaction_checkpoint_restores_raw_messages_and_clears_response_state() {
        let storage = Storage::in_memory().unwrap();
        let root = tempdir().unwrap();
        let session = storage.create_session(root.path()).unwrap();
        storage.append_message(&session, Role::User, "old").unwrap();
        storage
            .append_message(&session, Role::Assistant, "answer")
            .unwrap();
        storage
            .append_message(&session, Role::User, "latest")
            .unwrap();
        storage.save_response_id(&session, "resp").unwrap();
        assert_eq!(
            storage
                .compact_with_summary(&session, "goals and next step", 1)
                .unwrap(),
            2
        );
        assert!(storage.response_id(&session).unwrap().is_none());
        let compacted = storage.load_messages(&session).unwrap();
        assert!(
            compacted
                .iter()
                .any(|item| matches!(item, ConversationItem::CompactionSummary { .. }))
        );
        assert!(storage.restore_latest_compaction(&session).unwrap());
        assert!(storage.load_messages(&session).unwrap().iter().any(
            |item| matches!(item, ConversationItem::Message { content, .. } if content == "old")
        ));
        assert!(!storage.restore_latest_compaction(&session).unwrap());
    }

    #[test]
    fn supports_fork_undo_redo_and_compaction() {
        let storage = Storage::in_memory().unwrap();
        let root = tempdir().unwrap();
        let session = storage.create_session(root.path()).unwrap();
        storage.append_message(&session, Role::User, "one").unwrap();
        storage
            .append_message(&session, Role::Assistant, "answer")
            .unwrap();
        storage.append_message(&session, Role::User, "two").unwrap();
        assert!(storage.undo(&session).unwrap());
        assert_eq!(storage.load_messages(&session).unwrap().len(), 2);
        assert!(storage.redo(&session).unwrap());
        assert_eq!(storage.load_messages(&session).unwrap().len(), 3);
        assert!(storage.compact_session(&session, 1).unwrap() >= 1);
        let fork = storage.fork_session(&session).unwrap();
        assert_eq!(storage.load_messages(&fork).unwrap().len(), 1);
    }

    #[test]
    fn preserves_thinking_and_tool_order_for_display_restore() {
        let storage = Storage::in_memory().unwrap();
        let root = tempdir().unwrap();
        let session = storage.create_session(root.path()).unwrap();
        storage
            .append_message(&session, Role::User, "inspect")
            .unwrap();
        storage
            .append_thinking_summary(&session, "Checking the workspace")
            .unwrap();
        storage
            .append_message(&session, Role::Assistant, "I will inspect it.")
            .unwrap();
        let call = ToolCall {
            id: "call_1".into(),
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"src/lib.rs"}),
        };
        storage.append_tool_calls(&session, &[call]).unwrap();
        storage
            .append_tool_output(&session, "call_1", "contents")
            .unwrap();
        let items = storage.load_messages(&session).unwrap();
        assert!(matches!(items[1], ConversationItem::ThinkingSummary { .. }));
        assert!(matches!(
            items[3],
            ConversationItem::AssistantToolCalls { .. }
        ));
        assert!(matches!(items[4], ConversationItem::ToolOutput { .. }));
    }

    #[test]
    fn persists_provider_items_for_stateless_responses_replay() {
        let root = tempfile::tempdir().unwrap();
        let storage = Storage::open(&root.path().join("agent.db")).unwrap();
        let session = storage.create_session(root.path()).unwrap();
        storage
            .append_message(&session, Role::User, "search")
            .unwrap();
        storage
            .append_provider_item(
                &session,
                &serde_json::json!({
                    "id": "ws_1",
                    "type": "web_search_call",
                    "status": "completed",
                    "action": {"type":"search", "query":"Rust"}
                }),
            )
            .unwrap();
        let items = storage.load_messages(&session).unwrap();
        assert!(matches!(
            &items[1],
            ConversationItem::ProviderItem { item } if item["id"] == "ws_1"
        ));
    }
}
