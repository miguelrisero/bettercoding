ALTER TABLE workspaces ADD COLUMN archived_at DATETIME;

-- Existing archived workspaces predate an explicit archive timestamp. `updated_at`
-- is the best available one-time proxy because every workspace write, including
-- archive toggles, updates it; for untouched archived rows it is the archive time.
UPDATE workspaces
SET archived_at = updated_at
WHERE archived = TRUE;
