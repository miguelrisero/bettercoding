CREATE TABLE workspace_spawn_reservations (
    workspace_id   BLOB PRIMARY KEY,
    holder         TEXT NOT NULL CHECK (holder IN ('executor', 'cli')),
    fence          TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    expires_at     TEXT NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE INDEX idx_workspace_spawn_reservations_expiry
    ON workspace_spawn_reservations (expires_at);
