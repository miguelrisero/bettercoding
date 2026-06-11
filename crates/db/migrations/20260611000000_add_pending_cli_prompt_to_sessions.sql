-- CLI-first workspace creation: the initial prompt is no longer run by the
-- headless executor. It parks here until the workspace's CLI terminal opens,
-- whose tmux bootstrap hands it straight to the interactive claude.
ALTER TABLE sessions ADD COLUMN pending_cli_prompt TEXT;
