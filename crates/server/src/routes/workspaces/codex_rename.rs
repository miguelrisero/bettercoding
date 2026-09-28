//! Propagate a workspace rename into the workspace's Codex thread.
//!
//! A short-lived `codex app-server` sends `thread/name/set` for the selected
//! thread. Codex records the name in its thread index, where `codex resume`
//! and a later TUI session read it, and a thread forked later inherits it.
//! Nothing here writes Codex's files directly, and nothing is typed into a
//! live pane: its input can hold a draft that an injected command would
//! submit. A running TUI shows the new name after its next resume.
//! Everything is best-effort and runs off the request path.

use std::{process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use uuid::Uuid;

/// Bound on one `codex app-server` naming run, start-up included.
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(20);
const SET_NAME_REQUEST_ID: u64 = 2;

/// Codex thread ids are UUIDv7; Claude session ids are v4.
pub fn is_codex_thread_id(sid: &str) -> bool {
    Uuid::parse_str(sid).is_ok_and(|id| id.get_version_num() == 7)
}

pub async fn propagate_codex_rename(
    workspace_id: Uuid,
    thread_id: &str,
    name: &str,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(());
    }
    set_thread_name(thread_id, name).await?;
    tracing::debug!(%workspace_id, thread_id, "Codex thread renamed");
    Ok(())
}

/// Name `thread_id` through a short-lived `codex app-server`.
async fn set_thread_name(thread_id: &str, name: &str) -> Result<(), String> {
    let codex = utils::shell::resolve_executable_path("codex")
        .await
        .ok_or_else(|| "codex is not on PATH".to_string())?;
    let mut child = Command::new(codex)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn codex app-server: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("codex app-server has no stdin")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("codex app-server has no stdout")?;
    let exchange = async move {
        let requests = [
            json!({ "id": 1, "method": "initialize",
                    "params": { "clientInfo": { "name": "bettercoding", "version": "0" } } }),
            json!({ "method": "initialized" }),
            json!({ "id": SET_NAME_REQUEST_ID, "method": "thread/name/set",
                    "params": { "threadId": thread_id, "name": name } }),
        ];
        for request in requests {
            stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .map_err(|e| format!("write to codex app-server: {e}"))?;
        }
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| format!("read codex app-server: {e}"))?
        {
            if let Some(result) = set_name_outcome(&line) {
                return result;
            }
        }
        Err("codex app-server exited before answering thread/name/set".to_string())
    };
    let outcome = tokio::time::timeout(APP_SERVER_TIMEOUT, exchange).await;
    // Killed and reaped on every path, a timeout included.
    let _ = child.kill().await;
    outcome.map_err(|_| "codex app-server timed out".to_string())?
}

/// The outcome of `thread/name/set` when `line` is its response.
fn set_name_outcome(line: &str) -> Option<Result<(), String>> {
    let message: Value = serde_json::from_str(line).ok()?;
    if message.get("id").and_then(Value::as_u64) != Some(SET_NAME_REQUEST_ID) {
        return None;
    }
    Some(match message.get("error") {
        Some(error) => Err(format!("thread/name/set failed: {error}")),
        None => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_codex_thread_ids_take_the_codex_path() {
        assert!(is_codex_thread_id("01a0e8fe-4c9e-7d10-8700-c7d71eea010f"));
        assert!(!is_codex_thread_id("55be7663-103d-4f5f-bcaf-f30fc8b0249a"));
        assert!(!is_codex_thread_id("../01a0e8fe"));
    }

    #[test]
    fn only_the_set_name_response_settles_the_exchange() {
        assert!(set_name_outcome(r#"{"id":1,"result":{}}"#).is_none());
        assert!(set_name_outcome(r#"{"method":"thread/name/updated","params":{}}"#).is_none());
        assert!(set_name_outcome("not json").is_none());
        assert_eq!(set_name_outcome(r#"{"id":2,"result":{}}"#), Some(Ok(())));
        assert!(matches!(
            set_name_outcome(r#"{"id":2,"error":{"code":-32600,"message":"no thread"}}"#),
            Some(Err(_))
        ));
    }
}
