-- 004_reasoning.sql — keep the model's reasoning/thinking text.
--
-- Reasoning is stored so the user can read it and so exports keep the full
-- record. It is never replayed to the provider: `wire::message_json` only sends
-- `content`, so thinking text cannot grow the next request.
--
-- The workspace-root marker is also added here: it lets jAI decide whether a
-- stored session belongs to the directory it is being started in.

ALTER TABLE messages ADD COLUMN reasoning TEXT;

CREATE TABLE IF NOT EXISTS session_workspaces (
    session_id     TEXT PRIMARY KEY,
    workspace      TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_session_workspaces_path
    ON session_workspaces (workspace, updated_at DESC);
