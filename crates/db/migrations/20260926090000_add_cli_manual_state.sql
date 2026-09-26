-- A state the user sets on a workspace by hand, ported from Herdr's `! lock` /
-- `! seen`: `locked` parks the workspace (with an optional one-line note),
-- `seen` dismisses a finished turn. Engaging the session clears it; see
-- `CLEARING_EVENTS`.
ALTER TABLE workspace_cli_activity ADD COLUMN manual_kind TEXT;
ALTER TABLE workspace_cli_activity ADD COLUMN manual_note TEXT;
