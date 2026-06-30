-- Agentic-loop automation: keep a workspace's CLI agent going when a chat stops
-- for a NON-completion reason (usage limit / transient rate limit) by detecting
-- the limit banner in the tmux pane, scheduling a wake-up at the reset time (or
-- retrying every N minutes), and re-prompting the agent. Opt-in per workspace,
-- default OFF.

-- Per-workspace policy. A row exists only once the user has touched the toggle;
-- absence means "disabled" (the default).
CREATE TABLE IF NOT EXISTS loop_automations (
    workspace_id         BLOB PRIMARY KEY,
    -- Master switch. The supervisor only inspects enabled workspaces.
    enabled              INTEGER NOT NULL DEFAULT 0,
    -- How often to retry a transient rate-limit (seconds). 600 = every 10 min.
    retry_interval_secs  INTEGER NOT NULL DEFAULT 600,
    -- The message sent into the pane to nudge the agent to continue.
    continuation_prompt  TEXT NOT NULL DEFAULT 'Continue.',
    -- Safety cap on automatic re-prompts before the loop gives up (0 = no cap).
    max_attempts         INTEGER NOT NULL DEFAULT 50,
    -- Automatic re-prompts delivered so far (reset when re-enabled).
    attempts_used        INTEGER NOT NULL DEFAULT 0,
    created_at           TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at           TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

-- Pending / delivered wake-ups. A scheduled re-prompt for a workspace at a
-- specific time. `manual` rows are user-created ("ping at 05:00 UTC"); the
-- others are created by the supervisor on detecting a limit banner.
CREATE TABLE IF NOT EXISTS scheduled_wakeups (
    id            BLOB PRIMARY KEY,
    workspace_id  BLOB NOT NULL,
    -- When to fire (SQLite datetime string, UTC).
    fire_at       TEXT NOT NULL,
    -- 'rate_limit_retry' | 'usage_limit_wake' | 'manual'.
    kind          TEXT NOT NULL,
    -- Message to deliver; NULL falls back to the policy's continuation_prompt.
    prompt        TEXT,
    -- Which automatic attempt this is (for the cap / display).
    attempt       INTEGER NOT NULL DEFAULT 1,
    -- Set when the wake-up has been delivered (or skipped); NULL = pending.
    fired_at      TEXT,
    created_at    TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_scheduled_wakeups_due
    ON scheduled_wakeups (fired_at, fire_at);
CREATE INDEX IF NOT EXISTS idx_scheduled_wakeups_workspace
    ON scheduled_wakeups (workspace_id);
