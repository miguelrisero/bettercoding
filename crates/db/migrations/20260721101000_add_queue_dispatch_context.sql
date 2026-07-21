-- Retry metadata must survive durable queueing so the destructive reset and
-- transcript truncation happen only when the queued executor is accepted.
ALTER TABLE session_queued_messages
    ADD COLUMN dispatch_context TEXT;
