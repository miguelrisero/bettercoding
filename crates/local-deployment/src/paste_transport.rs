use async_trait::async_trait;
use services::services::cli_collab::CliPasteTransport;
use uuid::Uuid;

use crate::pty;

#[derive(Debug, Clone, Default)]
pub struct LocalCliPasteTransport;

#[async_trait]
impl CliPasteTransport for LocalCliPasteTransport {
    async fn paste_and_submit(&self, workspace_id: Uuid, program: &str, text: &str) -> bool {
        // Close the lease-to-paste TOCTOU window with a fresh pane-subtree
        // check under the send lock, immediately before the irreversible
        // keystroke injection. Only the agent the lease was derived for may
        // receive the text, and the send path picks that agent's delivery
        // (Claude's chunked paste stream, Codex's single bracketed paste).
        if !pty::CLI_AGENT_PROGRAMS.contains(&program) {
            return false;
        }
        let Some(target) = pty::locate_cli_tmux_target(workspace_id).await else {
            return false;
        };
        pty::send_cli_keys_to_live_agent(&target, &[program], text)
            .await
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
        Ok(pty::write_cli_resume_ready_file(workspace_id, sid)?)
    }
}
