-- Executor dispatch can legitimately take longer than terminal paste recovery.
-- A process-scoped owner lets reconciliation recover claims from a crashed
-- server without stealing a dispatch that is still in flight in this server.
ALTER TABLE session_queued_messages
    ADD COLUMN executor_claim_owner TEXT;
