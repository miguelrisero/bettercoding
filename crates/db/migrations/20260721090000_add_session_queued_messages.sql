-- Durable, single-slot collaboration queue. Terminal delivery remains active
-- through `pasted`; terminal states no longer occupy the session's slot.
CREATE TABLE session_queued_messages (
    id                  BLOB PRIMARY KEY,
    session_id          BLOB NOT NULL,
    prompt              TEXT NOT NULL,
    executor_config     TEXT,
    source              TEXT NOT NULL
                             CHECK (source IN ('ui', 'recovery')),
    state               TEXT NOT NULL
                             CHECK (state IN (
                                 'queued', 'pasting', 'pasted', 'imported',
                                 'failed', 'consumed', 'cancelled'
                             )),
    failure_reason      TEXT,
    claude_session_id   TEXT,
    pasted_at           TEXT,
    acked_at            TEXT,
    created_at          TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_session_queued_messages_active_session
    ON session_queued_messages (session_id)
    WHERE state IN ('queued', 'pasting', 'pasted');

CREATE INDEX idx_session_queued_messages_active_scan
    ON session_queued_messages (state, updated_at);
