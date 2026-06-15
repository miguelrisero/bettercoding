-- CLI-first workspace creation persists the model + reasoning effort the user
-- picked so the workspace's CLI terminal launches interactive claude with the
-- same selection. The headless executor that used to carry `executor_config`
-- no longer runs in this path, so without this the choice was silently dropped.
-- Both columns are nullable: chat/automation sessions fall back to the most
-- recent execution process's config, and an unset value defaults to Opus at
-- max effort for the initial CLI start.
ALTER TABLE sessions ADD COLUMN cli_model_id TEXT;
ALTER TABLE sessions ADD COLUMN cli_reasoning_id TEXT;
