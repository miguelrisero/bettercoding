use async_trait::async_trait;
use services::services::cli_collab::{CliPasteIdentity, CliPasteTransport};
use uuid::Uuid;

use crate::pty;

#[derive(Debug, Clone)]
pub struct LocalCliPasteTransport {
    pub pool: sqlx::SqlitePool,
}

#[async_trait]
impl CliPasteTransport for LocalCliPasteTransport {
    async fn paste_and_submit(
        &self,
        binding: &CliPasteIdentity,
        program: &str,
        text: &str,
    ) -> bool {
        // Close the lease-to-paste TOCTOU window with a fresh pane-subtree
        // check under the send lock, immediately before the irreversible
        // keystroke injection. Only the agent the lease was derived for may
        // receive the text, and the send path picks that agent's delivery
        // (Claude's chunked paste stream, Codex's single bracketed paste).
        if !pty::CLI_AGENT_PROGRAMS.contains(&program) {
            return false;
        }
        pty::send_cli_keys_for_lease(&self.pool, binding, program, text)
            .await
            .ok()
            .flatten()
            .is_some_and(pty::CliSendResult::delivered)
    }

    async fn pane_alive(&self, workspace_id: Uuid) -> anyhow::Result<bool> {
        Ok(pty::cli_tmux_session_exists_checked(workspace_id).await?)
    }

    async fn agent_running(&self, workspace_id: Uuid) -> Option<bool> {
        let target = pty::locate_cli_tmux_target(workspace_id).await?;
        pty::cli_pane_agent_program_at(&target, pty::CLI_AGENT_PROGRAMS)
            .await
            .map(|program| program.is_some())
    }

    async fn signal_resume_ready(&self, workspace_id: Uuid, sid: &str) -> anyhow::Result<()> {
        let _writer = pty::cli_writer_admission(&self.pool, workspace_id).await?;
        Ok(pty::write_cli_resume_ready_file(workspace_id, sid)?)
    }
}
