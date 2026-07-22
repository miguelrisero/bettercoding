ALTER TABLE cli_native_records
ADD COLUMN bound_queued_message_id BLOB
    REFERENCES session_queued_messages(id) ON DELETE SET NULL;

CREATE UNIQUE INDEX idx_cli_native_records_bound_queued_message
    ON cli_native_records (bound_queued_message_id)
    WHERE bound_queued_message_id IS NOT NULL;

ALTER TABLE claude_session_links
ADD COLUMN foreign_writer_seen_at TEXT;
