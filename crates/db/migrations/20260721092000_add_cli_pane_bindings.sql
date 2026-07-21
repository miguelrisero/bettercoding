CREATE TABLE cli_pane_bindings (
    id                  BLOB PRIMARY KEY,
    workspace_id        BLOB NOT NULL,
    session_id          BLOB NOT NULL,
    claude_session_id   TEXT,
    bound_via           TEXT NOT NULL
                             CHECK (bound_via IN ('cli-resume', 'cli-fresh')),
    created_at          TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    released_at         TEXT,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_cli_pane_bindings_active_workspace
    ON cli_pane_bindings (workspace_id)
    WHERE released_at IS NULL;

CREATE INDEX idx_cli_pane_bindings_active_session
    ON cli_pane_bindings (session_id)
    WHERE released_at IS NULL;
