-- Rebuild the Input family with foreign keys enabled. Child-first replacement
-- retains immutable history, notification claims, and the event cursor high-water.
CREATE TEMP TABLE workspace_input_expiry_high_water AS
    SELECT seq FROM sqlite_sequence WHERE name='workspace_input_events';
CREATE TABLE workspace_inputs_expiry (
    id TEXT PRIMARY KEY,
    workspace_id BLOB NOT NULL,
    external_key TEXT NOT NULL,
    creator TEXT NOT NULL,
    current_version INTEGER NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('open','answered','closed','cancelled','expired')),
    sequence INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    priority TEXT NOT NULL DEFAULT 'P2' CHECK(priority IN ('P0','P1','P2','P3','P4')),
    show_at TEXT,
    availability_generation TEXT,
    expires_at TEXT,
    UNIQUE(workspace_id, external_key),
    UNIQUE(workspace_id, id)
);
CREATE TABLE workspace_input_versions_expiry (
    workspace_id BLOB NOT NULL,
    input_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    title TEXT NOT NULL,
    html TEXT NOT NULL,
    contract TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    PRIMARY KEY(input_id, version),
    FOREIGN KEY(workspace_id, input_id) REFERENCES workspace_inputs_expiry(workspace_id, id)
);
CREATE TABLE workspace_input_events_expiry (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    workspace_id BLOB NOT NULL,
    input_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    actor_kind TEXT NOT NULL CHECK(actor_kind IN ('owner_ui','agent_mcp','system')),
    actor TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK(event_type IN ('created','version_published','answer_submitted','closed','cancelled','snoozed','available','expired')),
    version INTEGER NOT NULL,
    payload TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    hash TEXT NOT NULL,
    UNIQUE(input_id, sequence),
    UNIQUE(workspace_id, request_id),
    FOREIGN KEY(workspace_id, input_id) REFERENCES workspace_inputs_expiry(workspace_id, id)
);
CREATE TABLE workspace_input_availability_expiry (
    generation TEXT PRIMARY KEY,
    workspace_id BLOB NOT NULL,
    input_id TEXT NOT NULL,
    available_event_id TEXT UNIQUE,
    claim_token TEXT,
    claim_until TEXT,
    finished_at TEXT,
    FOREIGN KEY(generation) REFERENCES workspace_input_events_expiry(id),
    FOREIGN KEY(available_event_id) REFERENCES workspace_input_events_expiry(id),
    FOREIGN KEY(workspace_id, input_id) REFERENCES workspace_inputs_expiry(workspace_id, id)
);
INSERT INTO workspace_inputs_expiry SELECT *, NULL FROM workspace_inputs;
INSERT INTO workspace_input_versions_expiry SELECT * FROM workspace_input_versions;
INSERT INTO workspace_input_events_expiry SELECT * FROM workspace_input_events;
INSERT INTO workspace_input_availability_expiry SELECT * FROM workspace_input_availability;
DROP TABLE workspace_input_availability;
DROP TABLE workspace_input_events;
DROP TABLE workspace_input_versions;
DROP TABLE workspace_inputs;
ALTER TABLE workspace_inputs_expiry RENAME TO workspace_inputs;
ALTER TABLE workspace_input_versions_expiry RENAME TO workspace_input_versions;
ALTER TABLE workspace_input_events_expiry RENAME TO workspace_input_events;
ALTER TABLE workspace_input_availability_expiry RENAME TO workspace_input_availability;
UPDATE sqlite_sequence SET seq=max(seq, coalesce((SELECT seq FROM workspace_input_expiry_high_water), 0))
    WHERE name='workspace_input_events';
INSERT INTO sqlite_sequence(name, seq)
    SELECT 'workspace_input_events', seq FROM workspace_input_expiry_high_water
    WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name='workspace_input_events');
DROP TABLE workspace_input_expiry_high_water;
CREATE TRIGGER workspace_inputs_live_workspace BEFORE INSERT ON workspace_inputs
WHEN NOT EXISTS (SELECT 1 FROM workspaces WHERE id=NEW.workspace_id)
BEGIN SELECT RAISE(ABORT, 'input requires live workspace'); END;
CREATE TRIGGER workspace_inputs_identity_immutable BEFORE UPDATE OF id, workspace_id ON workspace_inputs
WHEN OLD.id != NEW.id OR OLD.workspace_id != NEW.workspace_id
BEGIN SELECT RAISE(ABORT, 'immutable input identity'); END;
CREATE INDEX workspace_inputs_scope ON workspace_inputs(workspace_id, id);
CREATE INDEX workspace_input_events_scope ON workspace_input_events(workspace_id, cursor);
CREATE TRIGGER workspace_input_versions_no_update BEFORE UPDATE ON workspace_input_versions BEGIN SELECT RAISE(ABORT, 'immutable input version'); END;
CREATE TRIGGER workspace_input_versions_no_delete BEFORE DELETE ON workspace_input_versions BEGIN SELECT RAISE(ABORT, 'immutable input version'); END;
CREATE TRIGGER workspace_input_events_no_update BEFORE UPDATE ON workspace_input_events BEGIN SELECT RAISE(ABORT, 'append-only input event'); END;
CREATE TRIGGER workspace_input_events_no_delete BEFORE DELETE ON workspace_input_events BEGIN SELECT RAISE(ABORT, 'append-only input event'); END;
CREATE INDEX workspace_inputs_due ON workspace_inputs(show_at, id)
    WHERE status='open' AND availability_generation IS NOT NULL;
CREATE INDEX workspace_inputs_expiring ON workspace_inputs(expires_at, id)
    WHERE status='open' AND expires_at IS NOT NULL;
CREATE INDEX workspace_input_availability_pending
    ON workspace_input_availability(claim_until, generation)
    WHERE available_event_id IS NOT NULL AND finished_at IS NULL;
