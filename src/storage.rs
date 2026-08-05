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

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
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

    #[cfg(test)]
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
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL
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
            INSERT OR IGNORE INTO schema_migrations(version, applied_at)
            VALUES (1, CURRENT_TIMESTAMP);
            ",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn create_session(&self, workspace: &Path) -> Result<String, StorageError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        self.lock()?.execute(
            "INSERT INTO sessions(id, workspace, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, workspace.display().to_string(), "New session", now],
        )?;
        Ok(id)
    }

    pub fn latest_session(&self, workspace: &Path) -> Result<Option<String>, StorageError> {
        self.lock()?
            .query_row(
                "SELECT id FROM sessions WHERE workspace = ?1 ORDER BY updated_at DESC LIMIT 1",
                [workspace.display().to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn append_message(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
    ) -> Result<(), StorageError> {
        let role = match role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO messages(session_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, role, content, now],
        )?;
        connection.execute(
            "UPDATE sessions SET updated_at = ?2, title = CASE WHEN title = 'New session' AND ?3 = 'user' THEN substr(?4, 1, 80) ELSE title END WHERE id = ?1",
            params![session_id, now, role, content],
        )?;
        Ok(())
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<ConversationItem>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id ASC")?;
        let rows = statement.query_map([session_id], |row| {
            let role: String = row.get(0)?;
            let role = match role.as_str() {
                "system" => Role::System,
                "assistant" => Role::Assistant,
                _ => Role::User,
            };
            Ok(ConversationItem::Message {
                role,
                content: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
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

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StorageError> {
        self.connection.lock().map_err(|_| StorageError::Poisoned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
    }
}
