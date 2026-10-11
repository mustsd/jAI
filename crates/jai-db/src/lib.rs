//! SQLite persistence for jAI.
//!
//! The database is treated as sensitive local data (`docs/security.md`): it is
//! created with owner-only permissions inside `~/.jai`, WAL mode is enabled so
//! a crash cannot destroy a conversation, and schema changes happen only
//! through the embedded migrations.
//!
//! Tool execution history is stored separately from the conversation so the
//! audit trail survives even if messages are cleared.

use jai_config::paths::{ensure_private_dir, restrict_file_to_owner};
use jai_core::error::{Error, Result};
use jai_core::message::{Message, Role, TokenUsage, ToolCall};
use jai_core::store::{
    InterruptedToolCall, Session, SessionStore, SessionSummary, ToolAudit, ToolAuditRecord, status,
};
use jai_core::time;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Embedded migrations, applied in order and recorded in `schema_migrations`.
pub const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../../migrations/001_initial.sql")),
    (2, include_str!("../../../migrations/002_tool_calls.sql")),
    (3, include_str!("../../../migrations/003_usage.sql")),
    (4, include_str!("../../../migrations/004_reasoning.sql")),
];

/// SQLite-backed [`SessionStore`].
pub struct SqliteStore {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl SqliteStore {
    /// Open (creating if needed) `~/.jai/jai.db`-style database files.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent).map_err(|err| {
                Error::storage(format!("could not prepare {}: {err}", parent.display()))
            })?;
        }
        let mut conn = Connection::open(path)
            .map_err(|err| Error::storage(format!("could not open {}: {err}", path.display())))?;
        configure(&conn)?;
        apply_migrations(&mut conn)?;
        restrict_file_to_owner(path)
            .map_err(|err| Error::storage(format!("could not secure {}: {err}", path.display())))?;
        // WAL creates sidecar files; keep them owner-only as well.
        for suffix in ["-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
            if sidecar.exists() {
                let _ = restrict_file_to_owner(&sidecar);
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
            path: Some(path.to_path_buf()),
        })
    }

    /// Open a throwaway in-memory database (tests).
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()
            .map_err(|err| Error::storage(format!("could not open in-memory database: {err}")))?;
        configure(&conn)?;
        apply_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    /// Database file path, when file-backed.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Run `PRAGMA integrity_check`; fails when SQLite reports damage.
    pub fn integrity_check(&self) -> Result<()> {
        let conn = self.lock()?;
        let result: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|err| Error::storage(format!("integrity check failed: {err}")))?;
        if result == "ok" {
            Ok(())
        } else {
            Err(Error::storage(format!("database is damaged: {result}")))
        }
    }

    /// Applied migration versions, in order.
    pub fn applied_migrations(&self) -> Result<Vec<i64>> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .map_err(storage_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(storage_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage_error)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| Error::storage("database lock poisoned"))
    }
}

fn storage_error(err: rusqlite::Error) -> Error {
    Error::storage(err.to_string())
}

fn configure(conn: &Connection) -> Result<()> {
    // WAL keeps readers working during a crash; busy_timeout avoids spurious
    // "database is locked" failures when two jAI processes overlap.
    let _mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(storage_error)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = NORMAL;
         PRAGMA trusted_schema = OFF;",
    )
    .map_err(storage_error)?;
    Ok(())
}

fn apply_migrations(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             applied_at INTEGER NOT NULL
         );",
    )
    .map_err(storage_error)?;

    for (version, sql) in MIGRATIONS {
        let applied: Option<i64> = conn
            .query_row(
                "SELECT version FROM schema_migrations WHERE version = ?1",
                params![version],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        if applied.is_some() {
            continue;
        }
        let transaction = conn.transaction().map_err(storage_error)?;
        transaction
            .execute_batch(sql)
            .map_err(|err| Error::storage(format!("migration {version} failed: {err}")))?;
        transaction
            .execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, time::unix_now()],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)?;
    }
    Ok(())
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

const SESSION_COLUMNS: &str = "id, title, provider, model, created_at, updated_at";

impl SessionStore for SqliteStore {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn create_session(&self, provider: &str, model: &str, title: Option<&str>) -> Result<Session> {
        let now = time::unix_now();
        let session = Session {
            id: jai_core::ids::session_id(),
            title: title.map(str::to_string),
            provider: provider.to_string(),
            model: model.to_string(),
            created_at: now,
            updated_at: now,
        };
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sessions (id, title, provider, model, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session.id,
                session.title,
                session.provider,
                session.model,
                session.created_at,
                session.updated_at
            ],
        )
        .map_err(storage_error)?;
        Ok(session)
    }

    fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.lock()?;
        conn.query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
            params![id],
            row_to_session,
        )
        .optional()
        .map_err(storage_error)
    }

    fn latest_session(&self) -> Result<Option<Session>> {
        let conn = self.lock()?;
        conn.query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions ORDER BY updated_at DESC LIMIT 1"),
            [],
            row_to_session,
        )
        .optional()
        .map_err(storage_error)
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT s.id, s.title, s.provider, s.model, s.created_at, s.updated_at,
                        (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id),
                        (SELECT MAX(m.created_at) FROM messages m WHERE m.session_id = s.id)
                 FROM sessions s
                 ORDER BY s.updated_at DESC",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(SessionSummary {
                    session: row_to_session(row)?,
                    message_count: row.get::<_, i64>(6)?.max(0) as u64,
                    last_message_at: row.get(7)?,
                })
            })
            .map_err(storage_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage_error)
    }

    fn delete_session(&self, id: &str) -> Result<bool> {
        let conn = self.lock()?;
        let removed = conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(storage_error)?;
        Ok(removed > 0)
    }

    fn rename_session(&self, id: &str, title: &str) -> Result<bool> {
        let conn = self.lock()?;
        let updated = conn
            .execute(
                "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, title, time::unix_now()],
            )
            .map_err(storage_error)?;
        Ok(updated > 0)
    }

    fn set_title_if_empty(&self, id: &str, title: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE sessions SET title = ?2
             WHERE id = ?1 AND (title IS NULL OR TRIM(title) = '')",
            params![id, title],
        )
        .map_err(storage_error)?;
        Ok(())
    }

    fn update_session_model(&self, id: &str, provider: &str, model: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE sessions SET provider = ?2, model = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, provider, model, time::unix_now()],
        )
        .map_err(storage_error)?;
        Ok(())
    }

    fn purge(&self) -> Result<u64> {
        let conn = self.lock()?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .map_err(storage_error)?;
        conn.execute_batch("DELETE FROM sessions;")
            .map_err(storage_error)?;
        Ok(count.max(0) as u64)
    }

    fn set_session_workspace(&self, session_id: &str, workspace: &str) -> Result<()> {
        let now = time::unix_now();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO session_workspaces (session_id, workspace, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(session_id) DO UPDATE
                 SET workspace = excluded.workspace, updated_at = excluded.updated_at",
            params![session_id, workspace, now],
        )
        .map_err(storage_error)?;
        Ok(())
    }

    fn session_workspace(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT workspace FROM session_workspaces WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)
    }

    fn latest_session_for_workspace(&self, workspace: &str) -> Result<Option<Session>> {
        let conn = self.lock()?;
        conn.query_row(
            &format!(
                "SELECT {} FROM sessions s
                 WHERE EXISTS (SELECT 1 FROM session_workspaces w
                                WHERE w.session_id = s.id AND w.workspace = ?1)
                 ORDER BY s.updated_at DESC LIMIT 1",
                SESSION_COLUMNS
                    .split(", ")
                    .map(|column| format!("s.{}", column.trim()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            params![workspace],
            row_to_session,
        )
        .optional()
        .map_err(storage_error)
    }

    fn append_message(&self, session_id: &str, message: &Message) -> Result<i64> {
        let tool_calls =
            if message.tool_calls.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&message.tool_calls).map_err(|err| {
                    Error::storage(format!("could not serialize tool calls: {err}"))
                })?)
            };
        let conn = self.lock()?;
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        if exists.is_none() {
            return Err(Error::storage(format!("unknown session `{session_id}`")));
        }
        conn.execute(
            "INSERT INTO messages
                (session_id, role, content, created_at, tool_calls, tool_call_id, tool_name, reasoning)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                message.role.as_str(),
                message.content,
                time::unix_now(),
                tool_calls,
                message.tool_call_id,
                message.name,
                message.reasoning
            ],
        )
        .map_err(storage_error)?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, time::unix_now()],
        )
        .map_err(storage_error)?;
        Ok(conn.last_insert_rowid())
    }

    fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT role, content, tool_calls, tool_call_id, tool_name, reasoning
                 FROM messages WHERE session_id = ?1 ORDER BY id ASC",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![session_id], |row| {
                let role: String = row.get(0)?;
                let tool_calls: Option<String> = row.get(2)?;
                Ok(Message {
                    role: Role::parse(&role).unwrap_or(Role::User),
                    content: row.get(1)?,
                    reasoning: row.get(5)?,
                    tool_calls: tool_calls
                        .and_then(|json| serde_json::from_str::<Vec<ToolCall>>(&json).ok())
                        .unwrap_or_default(),
                    tool_call_id: row.get(3)?,
                    name: row.get(4)?,
                })
            })
            .map_err(storage_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage_error)
    }

    fn clear_messages(&self, session_id: &str) -> Result<u64> {
        let conn = self.lock()?;
        let removed = conn
            .execute(
                "DELETE FROM messages WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(storage_error)?;
        Ok(removed as u64)
    }

    fn record_tool_start(&self, record: &ToolAuditRecord) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO tool_calls
                (session_id, message_id, call_id, tool_name, arguments, mode, risk, status, decision, summary, created_at)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.session_id,
                record.call.id,
                record.call.name,
                record.call.arguments,
                record.mode.to_string(),
                record.risk.to_string(),
                status::RUNNING,
                record.decision,
                record.summary,
                time::unix_now()
            ],
        )
        .map_err(storage_error)?;
        Ok(conn.last_insert_rowid())
    }

    fn finish_tool_call(&self, id: i64, status_label: &str, result: Option<&str>) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE tool_calls SET status = ?2 WHERE id = ?1",
            params![id, status_label],
        )
        .map_err(storage_error)?;
        if let Some(result) = result {
            conn.execute(
                "INSERT INTO tool_results (tool_call_id, result, created_at) VALUES (?1, ?2, ?3)",
                params![id, result, time::unix_now()],
            )
            .map_err(storage_error)?;
        }
        Ok(())
    }

    fn tool_audit(&self, session_id: &str) -> Result<Vec<ToolAudit>> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT t.id, t.session_id, t.tool_name, t.arguments, t.mode, t.risk, t.status,
                        t.decision, t.summary, t.created_at,
                        (SELECT r.result FROM tool_results r
                          WHERE r.tool_call_id = t.id ORDER BY r.id DESC LIMIT 1)
                 FROM tool_calls t WHERE t.session_id = ?1 ORDER BY t.id ASC",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![session_id], |row| {
                Ok(ToolAudit {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    tool_name: row.get(2)?,
                    arguments: row.get(3)?,
                    mode: row.get(4)?,
                    risk: row.get(5)?,
                    status: row.get(6)?,
                    decision: row.get(7)?,
                    summary: row.get(8)?,
                    created_at: row.get(9)?,
                    result: row.get(10)?,
                })
            })
            .map_err(storage_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage_error)
    }

    fn reconcile_interrupted(&self) -> Result<Vec<InterruptedToolCall>> {
        let conn = self.lock()?;
        let mut statement = conn
            .prepare(
                "SELECT id, session_id, tool_name, summary, created_at
                 FROM tool_calls WHERE status = ?1 ORDER BY id ASC",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![status::RUNNING], |row| {
                Ok(InterruptedToolCall {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    tool_name: row.get(2)?,
                    summary: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(storage_error)?;
        let interrupted: Vec<InterruptedToolCall> = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        if !interrupted.is_empty() {
            conn.execute(
                "UPDATE tool_calls SET status = ?2 WHERE status = ?1",
                params![status::RUNNING, status::INTERRUPTED],
            )
            .map_err(storage_error)?;
        }
        Ok(interrupted)
    }

    fn record_usage(&self, session_id: &str, model: &str, usage: &TokenUsage) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO usage (session_id, model, prompt_tokens, completion_tokens, total_tokens, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                model,
                usage.prompt_tokens as i64,
                usage.completion_tokens as i64,
                usage.total_tokens as i64,
                time::unix_now()
            ],
        )
        .map_err(storage_error)?;
        Ok(())
    }

    fn session_usage(&self, session_id: &str) -> Result<TokenUsage> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(completion_tokens), 0),
                    COALESCE(SUM(total_tokens), 0)
             FROM usage WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok(TokenUsage {
                    prompt_tokens: row.get::<_, i64>(0)?.max(0) as u64,
                    completion_tokens: row.get::<_, i64>(1)?.max(0) as u64,
                    total_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                })
            },
        )
        .map_err(storage_error)
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, time::unix_now()],
        )
        .map_err(storage_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jai_core::message::{PermissionMode, RiskLevel};

    fn store() -> SqliteStore {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.backend(), "sqlite");
        store
    }

    #[test]
    fn all_migrations_are_applied() {
        let store = store();
        let applied = store.applied_migrations().unwrap();
        assert_eq!(applied, vec![1, 2, 3]);
        store.integrity_check().unwrap();
    }

    #[test]
    fn sessions_round_trip() {
        let store = store();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        assert_eq!(session.provider, "deepseek");
        let loaded = store.get_session(&session.id).unwrap().unwrap();
        assert_eq!(loaded, session);
        assert_eq!(store.latest_session().unwrap().unwrap().id, session.id);

        store
            .set_title_if_empty(&session.id, "first title")
            .unwrap();
        store
            .set_title_if_empty(&session.id, "second title")
            .unwrap();
        assert_eq!(
            store
                .get_session(&session.id)
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("first title")
        );

        assert!(store.rename_session(&session.id, "renamed").unwrap());
        assert_eq!(
            store
                .get_session(&session.id)
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("renamed")
        );
        store
            .update_session_model(&session.id, "ollama", "llama3.2")
            .unwrap();
        let updated = store.get_session(&session.id).unwrap().unwrap();
        assert_eq!(updated.provider, "ollama");
        assert_eq!(updated.model, "llama3.2");

        assert!(store.delete_session(&session.id).unwrap());
        assert!(!store.delete_session(&session.id).unwrap());
        assert!(store.get_session(&session.id).unwrap().is_none());
    }

    #[test]
    fn messages_keep_tool_linkage_and_order() {
        let store = store();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store
            .append_message(&session.id, &Message::user("hi"))
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::assistant_with_tools(
                    "looking",
                    vec![ToolCall::new(
                        "call_1",
                        "filesystem_read",
                        "{\"path\":\"a\"}",
                    )],
                ),
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::tool("call_1", "filesystem_read", "contents"),
            )
            .unwrap();

        let messages = store.messages(&session.id).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_calls[0].name, "filesystem_read");
        assert_eq!(
            messages[1].tool_calls[0].arguments_value().unwrap()["path"],
            "a"
        );
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[2].name.as_deref(), Some("filesystem_read"));

        let unicode = "实现一个 agent";
        store
            .append_message(&session.id, &Message::assistant(unicode))
            .unwrap();
        let messages = store.messages(&session.id).unwrap();
        assert_eq!(messages[3].content, unicode);
    }

    #[test]
    fn summaries_count_messages_and_last_activity() {
        let store = store();
        let first = store
            .create_session("deepseek", "deepseek-chat", Some("one"))
            .unwrap();
        store
            .append_message(&first.id, &Message::user("a"))
            .unwrap();
        store
            .append_message(&first.id, &Message::assistant("b"))
            .unwrap();
        let second = store.create_session("ollama", "llama3.2", None).unwrap();

        let summaries = store.list_sessions().unwrap();
        assert_eq!(summaries.len(), 2);
        let first_summary = summaries
            .iter()
            .find(|summary| summary.session.id == first.id)
            .unwrap();
        assert_eq!(first_summary.message_count, 2);
        assert!(first_summary.last_message_at.is_some());
        let second_summary = summaries
            .iter()
            .find(|summary| summary.session.id == second.id)
            .unwrap();
        assert_eq!(second_summary.message_count, 0);
        assert!(second_summary.last_message_at.is_none());
    }

    #[test]
    fn audit_and_results_are_separate_from_messages() {
        let store = store();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        let id = store
            .record_tool_start(&ToolAuditRecord {
                session_id: session.id.clone(),
                call: ToolCall::new("call_1", "shell_execute", "{\"command\":\"ls\"}"),
                mode: PermissionMode::Safe,
                risk: RiskLevel::Execute,
                summary: "ls".to_string(),
                decision: "confirm".to_string(),
            })
            .unwrap();
        store
            .finish_tool_call(id, status::SUCCEEDED, Some("ok"))
            .unwrap();

        let audit = store.tool_audit(&session.id).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].mode, "safe");
        assert_eq!(audit[0].risk, "execute");
        assert_eq!(audit[0].decision.as_deref(), Some("confirm"));
        assert_eq!(audit[0].result.as_deref(), Some("ok"));
        assert!(store.messages(&session.id).unwrap().is_empty());
    }

    #[test]
    fn interrupted_tool_calls_are_detected_after_a_crash() {
        let store = store();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store
            .record_tool_start(&ToolAuditRecord {
                session_id: session.id.clone(),
                call: ToolCall::new("call_1", "shell_execute", "{}"),
                mode: PermissionMode::Yolo,
                risk: RiskLevel::Execute,
                summary: "cargo test".to_string(),
                decision: "allow".to_string(),
            })
            .unwrap();

        let interrupted = store.reconcile_interrupted().unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].tool_name, "shell_execute");
        assert_eq!(
            store.tool_audit(&session.id).unwrap()[0].status,
            status::INTERRUPTED
        );
        assert!(store.reconcile_interrupted().unwrap().is_empty());
    }

    #[test]
    fn usage_totals_accumulate() {
        let store = store();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store
            .record_usage(
                &session.id,
                "deepseek-chat",
                &TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    total_tokens: 30,
                },
            )
            .unwrap();
        store
            .record_usage(
                &session.id,
                "deepseek-chat",
                &TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 2,
                    total_tokens: 3,
                },
            )
            .unwrap();
        let total = store.session_usage(&session.id).unwrap();
        assert_eq!(total.prompt_tokens, 11);
        assert_eq!(total.completion_tokens, 22);
        assert_eq!(total.total_tokens, 33);
        assert_eq!(
            store.session_usage("missing").unwrap(),
            TokenUsage::default()
        );
    }

    #[test]
    fn settings_upsert() {
        let store = store();
        assert_eq!(store.setting("last_model").unwrap(), None);
        store.set_setting("last_model", "deepseek-chat").unwrap();
        store
            .set_setting("last_model", "deepseek-reasoner")
            .unwrap();
        assert_eq!(
            store.setting("last_model").unwrap().as_deref(),
            Some("deepseek-reasoner")
        );
    }

    #[test]
    fn clearing_and_purging_remove_conversations() {
        let store = store();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store
            .append_message(&session.id, &Message::user("a"))
            .unwrap();
        assert_eq!(store.clear_messages(&session.id).unwrap(), 1);
        assert!(store.messages(&session.id).unwrap().is_empty());

        store
            .append_message(&session.id, &Message::user("b"))
            .unwrap();
        assert_eq!(store.purge().unwrap(), 1);
        assert!(store.list_sessions().unwrap().is_empty());
        assert!(store.messages(&session.id).unwrap().is_empty());
    }

    #[test]
    fn appending_to_an_unknown_session_is_rejected() {
        let store = store();
        let err = store
            .append_message("missing", &Message::user("hi"))
            .unwrap_err();
        assert!(err.to_string().contains("unknown session"), "{err}");
    }

    #[test]
    fn deleting_a_session_removes_its_children() {
        let store = store();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store
            .append_message(&session.id, &Message::user("a"))
            .unwrap();
        let id = store
            .record_tool_start(&ToolAuditRecord {
                session_id: session.id.clone(),
                call: ToolCall::new("call_1", "shell_execute", "{}"),
                mode: PermissionMode::Safe,
                risk: RiskLevel::Execute,
                summary: "ls".to_string(),
                decision: "confirm".to_string(),
            })
            .unwrap();
        store
            .finish_tool_call(id, status::SUCCEEDED, Some("ok"))
            .unwrap();

        assert!(store.delete_session(&session.id).unwrap());
        assert!(store.messages(&session.id).unwrap().is_empty());
        assert!(store.tool_audit(&session.id).unwrap().is_empty());
    }
}
