-- Live state of a workspace's CLI-mode tmux claude session, maintained by the
-- backend tmux poller (CliActivityMonitor). Feeds the sidebar stage buckets:
-- 'running' lifts is_running, 'attention' marks claude-finished-while-away.
CREATE TABLE IF NOT EXISTS workspace_cli_activity (
    workspace_id BLOB PRIMARY KEY,
    state        TEXT NOT NULL DEFAULT 'idle',
    updated_at   TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);
