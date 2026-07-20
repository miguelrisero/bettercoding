-- Read-only Claude native transcript ingestion. The Claude store remains
-- untouched; these tables hold the app-owned copy, ownership bindings, and
-- the transactional publication log.

CREATE TABLE claude_session_links (
    claude_session_id  TEXT PRIMARY KEY,
    session_id         BLOB NOT NULL,
    workspace_id       BLOB NOT NULL,
    cwd                TEXT NOT NULL,
    bound_via          TEXT NOT NULL
                           CHECK (bound_via IN ('executor', 'cli-resume', 'cli-fresh', 'manual')),
    created_at         TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE INDEX idx_claude_session_links_session
    ON claude_session_links (session_id);
CREATE INDEX idx_claude_session_links_workspace
    ON claude_session_links (workspace_id);

-- One row represents one observed generation of a path. Replacements and
-- rewrites get a new generation (and id), so the raw-record primary key stays
-- immutable and INSERT OR IGNORE can safely make rescans idempotent.
CREATE TABLE cli_native_files (
    id                       BLOB PRIMARY KEY,
    claude_session_id        TEXT NOT NULL,
    dir_path                 TEXT NOT NULL,
    file_name                TEXT NOT NULL,
    -- Discovery scope only; ownership is exclusively claude_session_links.
    discovered_workspace_id  BLOB,
    dev                      INTEGER NOT NULL,
    inode                    INTEGER NOT NULL,
    generation               INTEGER NOT NULL DEFAULT 0,
    cursor_offset            INTEGER NOT NULL DEFAULT 0,
    next_line_seq            INTEGER NOT NULL DEFAULT 0,
    last_line_offset         INTEGER NOT NULL DEFAULT 0,
    last_line_hash           TEXT,
    observed_size            INTEGER NOT NULL DEFAULT 0,
    observed_mtime_ms        INTEGER,
    last_import_at           TEXT,
    created_at               TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at               TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    UNIQUE (dir_path, file_name, generation),
    FOREIGN KEY (discovered_workspace_id) REFERENCES workspaces(id) ON DELETE SET NULL
);

CREATE INDEX idx_cli_native_files_sid
    ON cli_native_files (claude_session_id);
CREATE INDEX idx_cli_native_files_workspace
    ON cli_native_files (discovered_workspace_id);

CREATE TABLE execution_native_links (
    execution_process_id  BLOB NOT NULL,
    native_uuid           TEXT NOT NULL,
    PRIMARY KEY (execution_process_id, native_uuid),
    FOREIGN KEY (execution_process_id) REFERENCES execution_processes(id) ON DELETE CASCADE
);

CREATE INDEX idx_execution_native_links_uuid
    ON execution_native_links (native_uuid);

CREATE TABLE cli_native_records (
    file_id                       BLOB NOT NULL,
    line_seq                      INTEGER NOT NULL,
    claude_session_id             TEXT NOT NULL,
    uuid                          TEXT,
    parent_uuid                   TEXT,
    kind                          TEXT NOT NULL,
    ts                            TEXT,
    raw                           TEXT NOT NULL,
    bound_coding_agent_turn_id    BLOB,
    PRIMARY KEY (file_id, line_seq),
    FOREIGN KEY (file_id) REFERENCES cli_native_files(id) ON DELETE CASCADE,
    FOREIGN KEY (bound_coding_agent_turn_id) REFERENCES coding_agent_turns(id) ON DELETE SET NULL
);

CREATE INDEX idx_cli_native_records_sid
    ON cli_native_records (claude_session_id, file_id, line_seq);
CREATE INDEX idx_cli_native_records_uuid
    ON cli_native_records (uuid);
CREATE UNIQUE INDEX idx_cli_native_records_bound_turn
    ON cli_native_records (bound_coding_agent_turn_id)
    WHERE bound_coding_agent_turn_id IS NOT NULL;

CREATE TABLE cli_ingest_outbox (
    session_id   BLOB NOT NULL,
    seq          INTEGER NOT NULL,
    file_id      BLOB NOT NULL,
    line_seq     INTEGER NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    PRIMARY KEY (session_id, seq),
    UNIQUE (session_id, file_id, line_seq),
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (file_id, line_seq) REFERENCES cli_native_records(file_id, line_seq) ON DELETE CASCADE
);

CREATE INDEX idx_cli_ingest_outbox_record
    ON cli_ingest_outbox (file_id, line_seq);
