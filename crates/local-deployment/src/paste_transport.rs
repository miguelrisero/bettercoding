use async_trait::async_trait;
use services::services::cli_collab::CliPasteTransport;
use uuid::Uuid;

use crate::pty;

#[derive(Debug, Clone, Default)]
pub struct LocalCliPasteTransport;

#[async_trait]
impl CliPasteTransport for LocalCliPasteTransport {
    async fn paste_and_submit(&self, workspace_id: Uuid, text: &str) -> bool {
        // Close the lease-to-paste TOCTOU window with a fresh pane-subtree
        // check immediately before the irreversible keystroke injection.
        if pty::cli_pane_agent_running(workspace_id, "claude").await != Some(true) {
            return false;
        }
        pty::send_cli_keys(workspace_id, text).await
    }

    async fn pane_alive(&self, workspace_id: Uuid) -> anyhow::Result<bool> {
        Ok(pty::cli_tmux_session_exists_checked(workspace_id).await?)
    }

    async fn agent_running(&self, workspace_id: Uuid) -> Option<bool> {
        pty::cli_pane_agent_running(workspace_id, "claude").await
    }

    async fn signal_resume_ready(&self, workspace_id: Uuid, sid: &str) -> anyhow::Result<()> {
        Ok(pty::write_cli_resume_ready_file(workspace_id, sid)?)
    }
}
