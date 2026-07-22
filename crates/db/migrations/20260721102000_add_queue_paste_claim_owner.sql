-- Paste delivery can outlast the startup grace just like executor dispatch.
-- A process-scoped owner lets reconciliation distinguish a live paste from a
-- claim abandoned by a crashed server.
ALTER TABLE session_queued_messages
    ADD COLUMN paste_claim_owner TEXT;
