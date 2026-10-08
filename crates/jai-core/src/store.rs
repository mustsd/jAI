//! Session/message persistence interface plus an in-memory implementation.
//!
//! The SQLite implementation lives in `jai-db`; keeping the trait here lets the
//! engine stay storage-agnostic and lets tests run without a database file.

use crate::error::{Error, Result};
use crate::ids;
use crate::message::{Message, PermissionMode, RiskLevel, Role, TokenUsage, ToolCall};
use crate::time;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One stored conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub provider: String,
    pub model: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Session {
    pub fn display_title(&self) -> &str {
        match self.title.as_deref() {
            Some(title) if !title.trim().is_empty() => title,
            _ => "(untitled)",
        }
    }
}

/// A session plus the numbers `jai sessions` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session: Session,
    pub message_count: u64,
    pub last_message_at: Option<i64>,
}

/// A tool call that was still `running` when the process exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedToolCall {
    pub id: i64,
    pub session_id: String,
    pub tool_name: String,
    pub summary: Option<String>,
    pub created_at: i64,
}

/// Audit record created before a tool runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAuditRecord {
    pub session_id: String,
    pub call: ToolCall,
    pub mode: PermissionMode,
    pub risk: RiskLevel,
    pub summary: String,
    /// Policy verdict that allowed execution (`allow` or `confirm`).
    pub decision: String,
}

/// Audit row as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAudit {
    pub id: i64,
    pub session_id: String,
    pub tool_name: String,
    pub arguments: String,
    pub mode: String,
    pub risk: String,
    pub status: String,
    pub decision: Option<String>,
    pub summary: Option<String>,
    pub result: Option<String>,
    pub created_at: i64,
}

/// Status values for the audit trail.
pub mod status {
    pub const RUNNING: &str = "running";
    pub const SUCCEEDED: &str = "succeeded";
    pub const FAILED: &str = "failed";
    pub const DENIED: &str = "denied";
    pub const INTERRUPTED: &str = "interrupted";
}

/// Persistence operations the engine needs.
pub trait SessionStore: Send + Sync {
    /// Backend name for display (`sqlite`, `memory`).
    fn backend(&self) -> &'static str;

    fn create_session(&self, provider: &str, model: &str, title: Option<&str>) -> Result<Session>;
    fn get_session(&self, id: &str) -> Result<Option<Session>>;
    fn latest_session(&self) -> Result<Option<Session>>;
    fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    fn delete_session(&self, id: &str) -> Result<bool>;
    fn rename_session(&self, id: &str, title: &str) -> Result<bool>;
    fn set_title_if_empty(&self, id: &str, title: &str) -> Result<()>;
    fn update_session_model(&self, id: &str, provider: &str, model: &str) -> Result<()>;
    fn purge(&self) -> Result<u64>;

    fn append_message(&self, session_id: &str, message: &Message) -> Result<i64>;
    fn messages(&self, session_id: &str) -> Result<Vec<Message>>;
    fn clear_messages(&self, session_id: &str) -> Result<u64>;

    fn record_tool_start(&self, record: &ToolAuditRecord) -> Result<i64>;
    fn finish_tool_call(&self, id: i64, status: &str, result: Option<&str>) -> Result<()>;
    fn tool_audit(&self, session_id: &str) -> Result<Vec<ToolAudit>>;
    fn reconcile_interrupted(&self) -> Result<Vec<InterruptedToolCall>>;

    fn record_usage(&self, session_id: &str, model: &str, usage: &TokenUsage) -> Result<()>;
    fn session_usage(&self, session_id: &str) -> Result<TokenUsage>;

    fn setting(&self, key: &str) -> Result<Option<String>>;
    fn set_setting(&self, key: &str, value: &str) -> Result<()>;
}

/// Session title derived from the first user message.
pub fn session_title_from(input: &str) -> String {
    const MAX_CHARS: usize = 60;
    let first_line = input
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if first_line.is_empty() {
        return "(untitled)".to_string();
    }
    if first_line.chars().count() <= MAX_CHARS {
        return first_line.to_string();
    }
    let mut title: String = first_line.chars().take(MAX_CHARS).collect();
    title.push('…');
    title
}

/// Markdown export used by `jai session export`.
pub fn session_markdown(session: &Session, messages: &[Message], audit: &[ToolAudit]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# jAI session {}\n\n", session.id));
    out.push_str(&format!("- Title: {}\n", session.display_title()));
    out.push_str(&format!("- Provider: {}\n", session.provider));
    out.push_str(&format!("- Model: {}\n", session.model));
    out.push_str(&format!(
        "- Created: {}\n",
        time::format_timestamp(session.created_at)
    ));
    out.push_str(&format!(
        "- Updated: {}\n\n",
        time::format_timestamp(session.updated_at)
    ));

    out.push_str("## Conversation\n\n");
    for message in messages {
        let label = match message.role {
            Role::System => "System",
            Role::User => "You",
            Role::Assistant => "jAI",
            Role::Tool => "Tool result",
        };
        out.push_str(&format!("### {label}\n\n"));
        let content = message.content.trim();
        if content.is_empty() {
            out.push_str("_(no text)_\n\n");
        } else {
            out.push_str(content);
            out.push_str("\n\n");
        }
        for call in &message.tool_calls {
            out.push_str(&format!(
                "> tool call `{}` args `{}`\n\n",
                call.name, call.arguments
            ));
        }
    }

    if !audit.is_empty() {
        out.push_str("## Tool activity (audit)\n\n");
        for entry in audit {
            out.push_str(&format!(
                "- {} `{}` [{} / {}] decision={} status={}{}\n",
                time::format_timestamp(entry.created_at),
                entry.tool_name,
                entry.mode,
                entry.risk,
                entry.decision.as_deref().unwrap_or("-"),
                entry.status,
                entry
                    .summary
                    .as_deref()
                    .map(|s| format!(" — {s}"))
                    .unwrap_or_default(),
            ));
        }
        out.push('\n');
    }
    out
}

/// In-memory store used when persistence is disabled and by engine tests.
#[derive(Debug, Default)]
pub struct MemoryStore {
    state: Mutex<MemoryState>,
}

#[derive(Debug, Default)]
struct MemoryState {
    sessions: BTreeMap<String, Session>,
    messages: BTreeMap<String, Vec<Message>>,
    audit: Vec<ToolAudit>,
    usage: BTreeMap<String, TokenUsage>,
    settings: BTreeMap<String, String>,
    next_audit_id: i64,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for MemoryStore {
    fn backend(&self) -> &'static str {
        "memory"
    }

    fn create_session(&self, provider: &str, model: &str, title: Option<&str>) -> Result<Session> {
        let now = time::unix_now();
        let session = Session {
            id: ids::session_id(),
            title: title.map(str::to_string),
            provider: provider.to_string(),
            model: model.to_string(),
            created_at: now,
            updated_at: now,
        };
        let mut state = self.lock()?;
        state.messages.insert(session.id.clone(), Vec::new());
        state.sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    fn get_session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self.lock()?.sessions.get(id).cloned())
    }

    fn latest_session(&self) -> Result<Option<Session>> {
        let state = self.lock()?;
        Ok(state
            .sessions
            .values()
            .max_by_key(|session| session.updated_at)
            .cloned())
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let state = self.lock()?;
        let mut summaries: Vec<SessionSummary> = state
            .sessions
            .values()
            .map(|session| {
                let messages = state
                    .messages
                    .get(&session.id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                SessionSummary {
                    session: session.clone(),
                    message_count: messages.len() as u64,
                    last_message_at: messages.last().map(|_| session.updated_at),
                }
            })
            .collect();
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.session.updated_at));
        Ok(summaries)
    }

    fn delete_session(&self, id: &str) -> Result<bool> {
        let mut state = self.lock()?;
        state.messages.remove(id);
        state.audit.retain(|entry| entry.session_id != id);
        Ok(state.sessions.remove(id).is_some())
    }

    fn rename_session(&self, id: &str, title: &str) -> Result<bool> {
        let mut state = self.lock()?;
        match state.sessions.get_mut(id) {
            Some(session) => {
                session.title = Some(title.to_string());
                session.updated_at = time::unix_now();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn set_title_if_empty(&self, id: &str, title: &str) -> Result<()> {
        let mut state = self.lock()?;
        if let Some(session) = state.sessions.get_mut(id) {
            if session
                .title
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                session.title = Some(title.to_string());
            }
        }
        Ok(())
    }

    fn update_session_model(&self, id: &str, provider: &str, model: &str) -> Result<()> {
        let mut state = self.lock()?;
        if let Some(session) = state.sessions.get_mut(id) {
            session.provider = provider.to_string();
            session.model = model.to_string();
            session.updated_at = time::unix_now();
        }
        Ok(())
    }

    fn purge(&self) -> Result<u64> {
        let mut state = self.lock()?;
        let count = state.sessions.len() as u64;
        state.sessions.clear();
        state.messages.clear();
        state.audit.clear();
        state.usage.clear();
        Ok(count)
    }

    fn append_message(&self, session_id: &str, message: &Message) -> Result<i64> {
        let mut state = self.lock()?;
        let session = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| Error::storage(format!("unknown session `{session_id}`")))?;
        session.updated_at = time::unix_now();
        let entry = state.messages.entry(session_id.to_string()).or_default();
        entry.push(message.clone());
        Ok(entry.len() as i64)
    }

    fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        Ok(self
            .lock()?
            .messages
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    fn clear_messages(&self, session_id: &str) -> Result<u64> {
        let mut state = self.lock()?;
        let removed = state
            .messages
            .get(session_id)
            .map(|messages| messages.len() as u64)
            .unwrap_or(0);
        state.messages.insert(session_id.to_string(), Vec::new());
        Ok(removed)
    }

    fn record_tool_start(&self, record: &ToolAuditRecord) -> Result<i64> {
        let mut state = self.lock()?;
        if !state.sessions.contains_key(&record.session_id) {
            return Err(Error::storage(format!(
                "unknown session `{}`",
                record.session_id
            )));
        }
        state.next_audit_id += 1;
        let id = state.next_audit_id;
        state.audit.push(ToolAudit {
            id,
            session_id: record.session_id.clone(),
            tool_name: record.call.name.clone(),
            arguments: record.call.arguments.clone(),
            mode: record.mode.to_string(),
            risk: record.risk.to_string(),
            status: status::RUNNING.to_string(),
            decision: Some(record.decision.clone()),
            summary: Some(record.summary.clone()),
            result: None,
            created_at: time::unix_now(),
        });
        Ok(id)
    }

    fn finish_tool_call(&self, id: i64, status: &str, result: Option<&str>) -> Result<()> {
        let mut state = self.lock()?;
        if let Some(entry) = state.audit.iter_mut().find(|entry| entry.id == id) {
            entry.status = status.to_string();
            if let Some(result) = result {
                entry.result = Some(result.to_string());
            }
        }
        Ok(())
    }

    fn tool_audit(&self, session_id: &str) -> Result<Vec<ToolAudit>> {
        Ok(self
            .lock()?
            .audit
            .iter()
            .filter(|entry| entry.session_id == session_id)
            .cloned()
            .collect())
    }

    fn reconcile_interrupted(&self) -> Result<Vec<InterruptedToolCall>> {
        let mut state = self.lock()?;
        let mut interrupted = Vec::new();
        for entry in state.audit.iter_mut() {
            if entry.status == status::RUNNING {
                entry.status = status::INTERRUPTED.to_string();
                interrupted.push(InterruptedToolCall {
                    id: entry.id,
                    session_id: entry.session_id.clone(),
                    tool_name: entry.tool_name.clone(),
                    summary: entry.summary.clone(),
                    created_at: entry.created_at,
                });
            }
        }
        Ok(interrupted)
    }

    fn record_usage(&self, session_id: &str, _model: &str, usage: &TokenUsage) -> Result<()> {
        let mut state = self.lock()?;
        state
            .usage
            .entry(session_id.to_string())
            .or_default()
            .add(usage);
        Ok(())
    }

    fn session_usage(&self, session_id: &str) -> Result<TokenUsage> {
        Ok(self
            .lock()?
            .usage
            .get(session_id)
            .copied()
            .unwrap_or_default())
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.lock()?.settings.get(key).cloned())
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.lock()?
            .settings
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
}

impl MemoryStore {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>> {
        self.state
            .lock()
            .map_err(|_| Error::storage("in-memory store lock poisoned"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_come_from_the_first_line_and_are_truncated() {
        assert_eq!(session_title_from("  Hello world\nsecond"), "Hello world");
        assert_eq!(session_title_from("\n\n   \n"), "(untitled)");
        let long = "x".repeat(200);
        let title = session_title_from(&long);
        assert_eq!(title.chars().count(), 61, "60 chars plus ellipsis");
        assert!(title.ends_with('…'));
        let unicode = "实现一个基于 Rust 的 AI 智能体，支持多提供商与模型的 OpenAI 兼容接口，并且提供命令行与网页两种交互方式。";
        assert_eq!(session_title_from(unicode).chars().count(), 61);
    }

    #[test]
    fn memory_store_round_trips_messages_and_audit() {
        let store = MemoryStore::new();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store
            .append_message(&session.id, &Message::user("hello"))
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

        let messages = store.messages(&session.id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);

        let audit = store.tool_audit(&session.id).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].status, status::SUCCEEDED);
        assert_eq!(audit[0].result.as_deref(), Some("ok"));
    }

    #[test]
    fn interrupted_tool_calls_are_reconciled_once() {
        let store = MemoryStore::new();
        let session = store.create_session("ollama", "llama3.2", None).unwrap();
        let id = store
            .record_tool_start(&ToolAuditRecord {
                session_id: session.id.clone(),
                call: ToolCall::new("call_1", "filesystem_write", "{}"),
                mode: PermissionMode::Yolo,
                risk: RiskLevel::Write,
                summary: "write Cargo.toml".to_string(),
                decision: "allow".to_string(),
            })
            .unwrap();
        let interrupted = store.reconcile_interrupted().unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].id, id);
        assert_eq!(interrupted[0].tool_name, "filesystem_write");
        assert!(store.reconcile_interrupted().unwrap().is_empty());
    }

    #[test]
    fn summaries_are_ordered_by_recency() {
        let store = MemoryStore::new();
        let first = store
            .create_session("deepseek", "deepseek-chat", Some("first"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = store.create_session("ollama", "llama3.2", None).unwrap();
        let summaries = store.list_sessions().unwrap();
        assert_eq!(summaries[0].session.id, second.id);
        assert_eq!(summaries[1].session.id, first.id);
        assert_eq!(summaries[1].session.display_title(), "first");
    }

    #[test]
    fn settings_and_usage_persist_in_memory() {
        let store = MemoryStore::new();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store.set_setting("last_model", "deepseek-chat").unwrap();
        assert_eq!(
            store.setting("last_model").unwrap().as_deref(),
            Some("deepseek-chat")
        );
        store
            .record_usage(
                &session.id,
                "deepseek-chat",
                &TokenUsage {
                    prompt_tokens: 3,
                    completion_tokens: 4,
                    total_tokens: 7,
                },
            )
            .unwrap();
        assert_eq!(store.session_usage(&session.id).unwrap().total_tokens, 7);
    }

    #[test]
    fn appending_to_an_unknown_session_fails() {
        let store = MemoryStore::new();
        let err = store
            .append_message("missing", &Message::user("hi"))
            .unwrap_err();
        assert!(err.to_string().contains("unknown session"));
    }

    #[test]
    fn markdown_export_contains_conversation_and_audit() {
        let store = MemoryStore::new();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store
            .append_message(&session.id, &Message::user("Explain ownership"))
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::assistant_with_tools(
                    "Looking it up.",
                    vec![ToolCall::new(
                        "call_1",
                        "filesystem_read",
                        "{\"path\":\"src/lib.rs\"}",
                    )],
                ),
            )
            .unwrap();
        let id = store
            .record_tool_start(&ToolAuditRecord {
                session_id: session.id.clone(),
                call: ToolCall::new("call_1", "filesystem_read", "{\"path\":\"src/lib.rs\"}"),
                mode: PermissionMode::Safe,
                risk: RiskLevel::ReadOnly,
                summary: "read src/lib.rs".to_string(),
                decision: "confirm".to_string(),
            })
            .unwrap();
        store
            .finish_tool_call(id, status::SUCCEEDED, Some("contents"))
            .unwrap();

        let session = store.get_session(&session.id).unwrap().unwrap();
        let markdown = session_markdown(
            &session,
            &store.messages(&session.id).unwrap(),
            &store.tool_audit(&session.id).unwrap(),
        );
        assert!(markdown.contains("# jAI session"));
        assert!(markdown.contains("### You"));
        assert!(markdown.contains("Explain ownership"));
        assert!(markdown.contains("tool call `filesystem_read`"));
        assert!(markdown.contains("## Tool activity (audit)"));
        assert!(markdown.contains("read src/lib.rs"));
    }
}
