-- 002_tool_calls.sql — tool execution history and audit trail.
-- This is deliberately separate from the model conversation: audit data must
-- stay readable even when the conversation is trimmed. See docs/security.md.

CREATE TABLE IF NOT EXISTS tool_calls (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    message_id  INTEGER,
    call_id     TEXT,
    tool_name   TEXT NOT NULL,
    arguments   TEXT NOT NULL DEFAULT '',
    mode        TEXT NOT NULL,
    risk        TEXT NOT NULL DEFAULT 'unknown',
    status      TEXT NOT NULL,
    decision    TEXT,
    summary     TEXT,
    created_at  INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tool_results (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    tool_call_id  INTEGER NOT NULL,
    result        TEXT,
    created_at    INTEGER NOT NULL,
    FOREIGN KEY (tool_call_id) REFERENCES tool_calls (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_tool_calls_session ON tool_calls (session_id, id);
CREATE INDEX IF NOT EXISTS idx_tool_calls_status ON tool_calls (status);
CREATE INDEX IF NOT EXISTS idx_tool_results_call ON tool_results (tool_call_id);
