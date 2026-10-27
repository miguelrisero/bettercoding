CREATE TABLE workspace_inputs (
    id TEXT PRIMARY KEY,
    workspace_id BLOB NOT NULL REFERENCES workspaces(id),
    external_key TEXT NOT NULL,
    creator TEXT NOT NULL,
    current_version INTEGER NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('open','answered','closed','cancelled')),
    sequence INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(workspace_id, external_key),
    UNIQUE(workspace_id, id)
);
CREATE TABLE workspace_input_versions (
    workspace_id BLOB NOT NULL,
    input_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    title TEXT NOT NULL,
    html TEXT NOT NULL,
    contract TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    PRIMARY KEY(input_id, version),
    FOREIGN KEY(workspace_id, input_id) REFERENCES workspace_inputs(workspace_id, id)
);
CREATE TABLE workspace_input_events (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    workspace_id BLOB NOT NULL,
    input_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    actor_kind TEXT NOT NULL CHECK(actor_kind IN ('owner_ui','agent_mcp','system')),
    actor TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK(event_type IN ('created','version_published','answer_submitted','closed','cancelled')),
    version INTEGER NOT NULL,
    payload TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    hash TEXT NOT NULL,
    UNIQUE(input_id, sequence),
    UNIQUE(workspace_id, request_id),
    FOREIGN KEY(workspace_id, input_id) REFERENCES workspace_inputs(workspace_id, id)
);
CREATE INDEX workspace_inputs_scope ON workspace_inputs(workspace_id, id);
CREATE INDEX workspace_input_events_scope ON workspace_input_events(workspace_id, cursor);
CREATE TRIGGER workspace_input_versions_no_update BEFORE UPDATE ON workspace_input_versions BEGIN SELECT RAISE(ABORT, 'immutable input version'); END;
CREATE TRIGGER workspace_input_versions_no_delete BEFORE DELETE ON workspace_input_versions BEGIN SELECT RAISE(ABORT, 'immutable input version'); END;
CREATE TRIGGER workspace_input_events_no_update BEFORE UPDATE ON workspace_input_events BEGIN SELECT RAISE(ABORT, 'append-only input event'); END;
CREATE TRIGGER workspace_input_events_no_delete BEFORE DELETE ON workspace_input_events BEGIN SELECT RAISE(ABORT, 'append-only input event'); END;
