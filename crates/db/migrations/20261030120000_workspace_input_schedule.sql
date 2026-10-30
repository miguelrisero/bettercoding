ALTER TABLE workspace_inputs ADD COLUMN show_at TEXT;
-- NULL leaves pre-migration Inputs visible without retrospective events or work.
ALTER TABLE workspace_inputs ADD COLUMN availability_generation TEXT;
CREATE INDEX workspace_inputs_due ON workspace_inputs(show_at, id)
    WHERE status='open' AND availability_generation IS NOT NULL;

-- Rebuild only the event CHECK constraint. Preserve rows, cursors, and the
-- AUTOINCREMENT high-water value, including any gaps above the retained rows.
CREATE TEMP TABLE workspace_input_event_high_water AS
    SELECT seq FROM sqlite_sequence WHERE name='workspace_input_events';
CREATE TABLE workspace_input_events_scheduled (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    workspace_id BLOB NOT NULL,
    input_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    actor_kind TEXT NOT NULL CHECK(actor_kind IN ('owner_ui','agent_mcp','system')),
    actor TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK(event_type IN ('created','version_published','answer_submitted','closed','cancelled','snoozed','available')),
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
INSERT INTO workspace_input_events_scheduled SELECT * FROM workspace_input_events;
DROP TABLE workspace_input_events;
ALTER TABLE workspace_input_events_scheduled RENAME TO workspace_input_events;
UPDATE sqlite_sequence SET seq=max(seq, coalesce((SELECT seq FROM workspace_input_event_high_water), 0))
    WHERE name='workspace_input_events';
-- An empty ledger can still have a retained high-water value.
INSERT INTO sqlite_sequence(name, seq)
    SELECT 'workspace_input_events', seq FROM workspace_input_event_high_water
    WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name='workspace_input_events');
DROP TABLE workspace_input_event_high_water;
CREATE INDEX workspace_input_events_scope ON workspace_input_events(workspace_id, cursor);
CREATE TRIGGER workspace_input_events_no_update BEFORE UPDATE ON workspace_input_events BEGIN SELECT RAISE(ABORT, 'append-only input event'); END;
CREATE TRIGGER workspace_input_events_no_delete BEFORE DELETE ON workspace_input_events BEGIN SELECT RAISE(ABORT, 'append-only input event'); END;

-- One record per availability generation. Claims expire after process failure.
-- Delivery is best effort: a crash after OS display can repeat a notification.
CREATE TABLE workspace_input_availability (
    generation TEXT PRIMARY KEY,
    workspace_id BLOB NOT NULL,
    input_id TEXT NOT NULL,
    available_event_id TEXT UNIQUE,
    claim_token TEXT,
    claim_until TEXT,
    finished_at TEXT,
    FOREIGN KEY(generation) REFERENCES workspace_input_events(id),
    FOREIGN KEY(available_event_id) REFERENCES workspace_input_events(id),
    FOREIGN KEY(workspace_id, input_id) REFERENCES workspace_inputs(workspace_id, id)
);
CREATE INDEX workspace_input_availability_pending
    ON workspace_input_availability(claim_until, generation)
    WHERE available_event_id IS NOT NULL AND finished_at IS NULL;
