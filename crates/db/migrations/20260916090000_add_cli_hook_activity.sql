-- Claude Code's own hook events, as reported by a CLI-mode session.
--
-- `state` stays the 3-bucket sidebar projection the tmux poller maintains; the
-- columns below carry the richer phase the agent reports about itself, plus the
-- session identity the rename integration needs. All nullable: a workspace whose
-- agent never reports (codex, gemini, a claude too old for the hook) keeps the
-- poller's guess and a NULL phase.
ALTER TABLE workspace_cli_activity ADD COLUMN phase TEXT;
ALTER TABLE workspace_cli_activity ADD COLUMN tasks INTEGER;
ALTER TABLE workspace_cli_activity ADD COLUMN crons INTEGER;
ALTER TABLE workspace_cli_activity ADD COLUMN agent_session_id TEXT;
ALTER TABLE workspace_cli_activity ADD COLUMN transcript_path TEXT;
-- Monotonic nanosecond stamp from the reporting hook; older reports are dropped.
ALTER TABLE workspace_cli_activity ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workspace_cli_activity ADD COLUMN hook_at TEXT;
