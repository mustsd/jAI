-- 003_usage.sql — token usage, local settings, and tool linkage on messages.

CREATE TABLE IF NOT EXISTS usage (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id         TEXT NOT NULL,
    model              TEXT NOT NULL,
    prompt_tokens      INTEGER NOT NULL DEFAULT 0,
    completion_tokens  INTEGER NOT NULL DEFAULT 0,
    total_tokens       INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_usage_session ON usage (session_id);

-- Local defaults (last used provider/model, permission mode, ...).
-- API keys are never stored here: see docs/security.md.
CREATE TABLE IF NOT EXISTS settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- Assistant messages may carry tool calls; tool results reference the call id.
ALTER TABLE messages ADD COLUMN tool_calls TEXT;
ALTER TABLE messages ADD COLUMN tool_call_id TEXT;
ALTER TABLE messages ADD COLUMN tool_name TEXT;
