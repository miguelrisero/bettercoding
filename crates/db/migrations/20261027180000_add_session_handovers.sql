CREATE TABLE session_handovers (
    operation_id BLOB PRIMARY KEY NOT NULL,
    workspace_id BLOB NOT NULL REFERENCES workspaces(id),
    request_id TEXT NOT NULL,
    request_json TEXT NOT NULL,
    predecessor_session_id BLOB NOT NULL REFERENCES sessions(id),
    successor_session_id BLOB NOT NULL REFERENCES sessions(id),
    predecessor_binding_id BLOB NOT NULL,
    binding_id BLOB NOT NULL,
    successor_native_session_id TEXT NOT NULL,
    fallback_session_id TEXT NOT NULL,
    capability TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('starting', 'awaiting_confirmation', 'confirmed', 'rolled_back', 'failed')),
    deadline_at TEXT NOT NULL,
    confirmed_at TEXT,
    failure TEXT,
    rollback_started INTEGER NOT NULL DEFAULT 0,
    launch_json TEXT NOT NULL,
    UNIQUE(workspace_id, request_id)
);
CREATE UNIQUE INDEX session_handovers_one_open ON session_handovers(workspace_id)
    WHERE phase IN ('starting', 'awaiting_confirmation');
