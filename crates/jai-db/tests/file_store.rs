//! File-backed storage tests: permissions, migrations and durability.

use jai_core::message::{Message, PermissionMode, RiskLevel, ToolCall};
use jai_core::store::{SessionStore, ToolAuditRecord, status};
use jai_db::SqliteStore;
use std::path::PathBuf;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("jai-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn db(&self) -> PathBuf {
        self.path.join("jai.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn opening_creates_the_database_and_applies_every_migration() {
    let dir = TempDir::new("create");
    let store = SqliteStore::open(&dir.db()).unwrap();
    assert_eq!(store.applied_migrations().unwrap(), vec![1, 2, 3]);
    store.integrity_check().unwrap();
    assert_eq!(store.path(), Some(dir.db().as_path()));
    assert!(dir.db().exists());
}

#[test]
fn reopening_preserves_data_and_does_not_rerun_migrations() {
    let dir = TempDir::new("reopen");
    let session_id = {
        let store = SqliteStore::open(&dir.db()).unwrap();
        let session = store
            .create_session("deepseek", "deepseek-chat", Some("kept"))
            .unwrap();
        store
            .append_message(&session.id, &Message::user("实现一个 agent"))
            .unwrap();
        session.id
    };

    let reopened = SqliteStore::open(&dir.db()).unwrap();
    assert_eq!(reopened.applied_migrations().unwrap(), vec![1, 2, 3]);
    let session = reopened.get_session(&session_id).unwrap().unwrap();
    assert_eq!(session.display_title(), "kept");
    assert_eq!(
        reopened.messages(&session_id).unwrap()[0].content,
        "实现一个 agent"
    );
}

#[test]
fn a_crash_during_a_tool_call_is_recoverable() {
    let dir = TempDir::new("crash");
    let session_id = {
        let store = SqliteStore::open(&dir.db()).unwrap();
        let session = store
            .create_session("deepseek", "deepseek-chat", None)
            .unwrap();
        store
            .append_message(&session.id, &Message::user("run the tests"))
            .unwrap();
        // Simulates a process that died after the audit row was written but
        // before the tool finished.
        store
            .record_tool_start(&ToolAuditRecord {
                session_id: session.id.clone(),
                call: ToolCall::new("call_1", "shell_execute", "{\"command\":\"cargo test\"}"),
                mode: PermissionMode::Safe,
                risk: RiskLevel::Execute,
                summary: "cargo test".to_string(),
                decision: "confirm".to_string(),
            })
            .unwrap();
        session.id
    };

    let reopened = SqliteStore::open(&dir.db()).unwrap();
    let interrupted = reopened.reconcile_interrupted().unwrap();
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].session_id, session_id);
    assert_eq!(interrupted[0].tool_name, "shell_execute");
    assert_eq!(interrupted[0].summary.as_deref(), Some("cargo test"));
    assert_eq!(
        reopened.tool_audit(&session_id).unwrap()[0].status,
        status::INTERRUPTED
    );
}

#[cfg(unix)]
#[test]
fn the_database_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("perms");
    let store = SqliteStore::open(&dir.db()).unwrap();
    store
        .create_session("deepseek", "deepseek-chat", None)
        .unwrap();

    let file_mode = std::fs::metadata(dir.db()).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600, "database must not be world readable");
    let dir_mode = std::fs::metadata(&dir.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "state directory must be owner-only");
}

#[test]
fn concurrent_processes_can_share_the_database() {
    let dir = TempDir::new("shared");
    let first = SqliteStore::open(&dir.db()).unwrap();
    let second = SqliteStore::open(&dir.db()).unwrap();
    let session = first
        .create_session("deepseek", "deepseek-chat", None)
        .unwrap();
    // The second handle sees the first handle's committed data (WAL readers).
    assert!(second.get_session(&session.id).unwrap().is_some());
    second
        .append_message(&session.id, &Message::user("from the second"))
        .unwrap();
    assert_eq!(first.messages(&session.id).unwrap().len(), 1);
}
