//! Propagate a workspace rename into the running Claude Code session.
//!
//! Claude Code persists a conversation title in two places it reads back:
//! a `custom-title.json` sidecar next to the transcript, and a
//! `"type":"custom-title"` record appended to the transcript itself. Writing
//! both is durable and cannot collide with anything the user has half-typed
//! in the pane. The live TUI header additionally updates when the session
//! processes the `/rename` command, so keystrokes are also injected — but
//! only when the pane demonstrably runs claude and its clients have been
//! input-idle: `send_cli_keys` types into the TUI's input box and submits,
//! so an ungated inject would append to a half-typed draft and submit it as
//! a prompt. Idleness is judged from tmux's `client_activity` (last client
//! input); if it cannot be established, the keystrokes are skipped and the
//! files carry the rename alone.
//!
//! Everything here is best-effort and runs OFF the request path: a rename
//! must succeed even when no Claude session, transcript, or tmux exists.
//!
//! Known ceiling: clearing a workspace's name (rename to empty) stores NULL
//! and skips propagation, and claude has no "untitled" record to write —
//! a previously set session title survives until the next non-empty rename.
//! ponytail: needs a claude-side "clear title" mechanism that does not
//! exist; revisit if one appears.

use std::path::{Path, PathBuf};

use db::models::{
    claude_session_link::ClaudeSessionLink, coding_agent_turn::CodingAgentTurn, session::Session,
};
use local_deployment::pty::{
    cli_pane_agent_running_at, latest_cli_client_activity, locate_cli_tmux_target, now_unix_secs,
    send_cli_keys_to,
};
use sqlx::SqlitePool;
use uuid::Uuid;

/// How long the newest tmux client activity on the workspace's CLI socket must
/// be old before `/rename` keystrokes may be injected. The gate is
/// socket-scoped (list-clients has no per-pane filter), so typing in ANY
/// workspace's pane on the shared socket suppresses injection — deliberately
/// over-conservative, never fail-open. Generous on purpose: it must exceed
/// any human pause mid-thought while composing a prompt, because injecting
/// during composition is exactly the corruption this gate exists to prevent.
const RENAME_IDLE_MARGIN_SECS: i64 = 90;

/// Sidecar path claude reads the custom title from:
/// `<transcript_path minus .jsonl>/custom-title.json`.
pub fn custom_title_sidecar_path(transcript_path: &Path) -> Option<PathBuf> {
    let file_name = transcript_path.file_name()?.to_str()?;
    let stem = file_name.strip_suffix(".jsonl")?;
    let dir = transcript_path.parent()?;
    Some(dir.join(stem).join("custom-title.json"))
}

/// Whether `/rename` keystrokes may be injected: the name must survive being
/// typed into a single TUI input line as literal text, and the newest tmux
/// `client_activity` on the socket must be older than the margin. Rejects:
/// control bytes (send-keys -l types them raw into the pane's pty, where a
/// raw-mode TUI reads Esc/Ctrl-C/... as real keystrokes — a name containing
/// one would drive the TUI, not type into it); newlines (submit mid-name);
/// empty names. No readable client activity means idleness could not be
/// established — fail closed, skip.
pub fn should_inject_rename_keys(
    name: &str,
    latest_client_activity: Option<i64>,
    now: i64,
) -> bool {
    if name.chars().any(|c| c.is_control() || c == '\u{7f}') || name.is_empty() {
        return false;
    }
    let Some(activity) = latest_client_activity else {
        return false;
    };
    now.saturating_sub(activity) >= RENAME_IDLE_MARGIN_SECS
}

/// Entry point: spawn (never await) after a successful workspace rename.
/// Owns its own error handling; nothing here can fail the rename.
pub fn spawn_rename_propagation(pool: SqlitePool, workspace_id: Uuid, name: String) {
    tokio::spawn(async move {
        if let Err(error) = propagate_rename(&pool, workspace_id, &name).await {
            tracing::warn!(
                %workspace_id,
                error,
                "Claude session rename propagation failed (rename itself succeeded)"
            );
        }
    });
}

async fn propagate_rename(pool: &SqlitePool, workspace_id: Uuid, name: &str) -> Result<(), String> {
    let Some(session) = Session::find_latest_by_workspace_id(pool, workspace_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(()); // no session: nothing running to rename
    };

    // Same precedence as the attach path (cli_agent.rs): executor-reported
    // turns first, the CLI binding second. Both resolve to a claude
    // session id; neither is guaranteed to exist.
    let claude_session_id = match CodingAgentTurn::find_latest_session_info(pool, session.id)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(info) => Some(info.session_id),
        None => ClaudeSessionLink::find_latest_for_session(pool, session.id)
            .await
            .map_err(|e| e.to_string())?
            .map(|link| link.claude_session_id),
    };
    let Some(claude_session_id) = claude_session_id else {
        return Ok(()); // no claude conversation bound: nothing to rename
    };

    // The sid comes from agent-reported data, so it is untrusted for path
    // construction: require a plain UUID (claude's own session ids are UUIDs)
    // before it may steer any write below.
    if !is_valid_claude_session_id(&claude_session_id) {
        return Ok(());
    }

    let Some(transcript_path) = find_transcript_path(&claude_session_id) else {
        return Ok(()); // no transcript on disk (headless-only or pruned)
    };
    if !transcript_path.is_file() {
        return Ok(()); // gone or never written: sidecar dir would be orphaned
    }

    write_sidecar(&transcript_path, &claude_session_id, name)?;
    append_transcript_record(&transcript_path, &claude_session_id, name)?;

    // Keystrokes go only to a pane that is demonstrably running claude: a
    // workspace whose live agent is codex (but that has an older claude
    // transcript) would otherwise receive "/rename ..." as a prompt. The
    // target is located once and reused for the send, so a mid-delivery home
    // flip cannot redirect it.
    if let Some(target) = locate_cli_tmux_target(workspace_id).await
        && cli_pane_agent_running_at(&target, "claude").await == Some(true)
        && let Some(latest_activity) = latest_cli_client_activity(&target).await
        && should_inject_rename_keys(name, Some(latest_activity), now_unix_secs())
    {
        let command = format!("/rename {name}");
        if !send_cli_keys_to(&target, &command).await {
            tracing::debug!(
                %workspace_id,
                "Claude /rename keystrokes not delivered; files carry the rename"
            );
        }
    }
    Ok(())
}

/// The sid is agent-reported (DB-sourced) and steers every write below, so
/// only a plain UUID may pass — no separators, no traversal, no suffixes.
fn is_valid_claude_session_id(claude_session_id: &str) -> bool {
    Uuid::parse_str(claude_session_id).is_ok()
}

/// Locate `<projects-dir>/<project-key>/<sid>.jsonl` for a claude session id.
/// The projects root mirrors the transcript ingest service's derivation.
fn find_transcript_path(claude_session_id: &str) -> Option<PathBuf> {
    let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
    // `flatten` twice: one unreadable directory entry must skip that entry,
    // not abort the scan for every workspace after it.
    let entries = std::fs::read_dir(projects_dir).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(format!("{claude_session_id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Write `{"customTitle":"<name>"}` next to the transcript, atomically
/// (temp file + rename) so a concurrent claude read never sees a torn file.
/// Overwriting is the intended operation: the sidecar holds only the title.
/// Dir and file are created 0700/0600 to match the project's private-file
/// convention (`write_private_file` in local-deployment pty).
fn write_sidecar(
    transcript_path: &Path,
    claude_session_id: &str,
    name: &str,
) -> Result<(), String> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    let Some(sidecar_path) = custom_title_sidecar_path(transcript_path) else {
        return Err("transcript path has no derivable sidecar".to_string());
    };
    let dir = sidecar_path
        .parent()
        .ok_or_else(|| "sidecar path has no parent".to_string())?;
    #[cfg(unix)]
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("create sidecar dir: {e}"))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).map_err(|e| format!("create sidecar dir: {e}"))?;

    let payload = serde_json::json!({ "customTitle": name });
    // Pid-suffixed so two racing propagations (a rapid double-rename) cannot
    // interleave on one temp file.
    let temp_path =
        sidecar_path.with_file_name(format!("custom-title.json.{}.tmp", std::process::id()));
    #[cfg(unix)]
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp_path)
            .map_err(|e| format!("write sidecar temp for {claude_session_id}: {e}"))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("set sidecar temp mode: {e}"))?;
        file.write_all(payload.to_string().as_bytes())
            .map_err(|e| format!("write sidecar temp for {claude_session_id}: {e}"))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&temp_path, payload.to_string())
        .map_err(|e| format!("write sidecar temp for {claude_session_id}: {e}"))?;
    std::fs::rename(&temp_path, &sidecar_path)
        .map_err(|e| format!("rename sidecar into place for {claude_session_id}: {e}"))?;
    Ok(())
}

/// Append the transcript record claude re-reads to learn the new title.
/// Append-only, one line, never opened for writing elsewhere: the ingest
/// tailer treats the unknown `custom-title` kind as an ignorable record.
fn append_transcript_record(
    transcript_path: &Path,
    claude_session_id: &str,
    name: &str,
) -> Result<(), String> {
    use std::io::Write;
    let record = serde_json::json!({
        "type": "custom-title",
        "customTitle": name,
        "sessionId": claude_session_id,
    });
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(transcript_path)
        .map_err(|e| format!("open transcript for append: {e}"))?;
    writeln!(file, "{record}").map_err(|e| format!("append custom-title record: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_derivation() {
        let transcript = Path::new("/home/u/.claude/projects/-tmp-x/abc-123/abc-123.jsonl");
        assert_eq!(
            custom_title_sidecar_path(transcript).as_deref(),
            Some(Path::new(
                "/home/u/.claude/projects/-tmp-x/abc-123/abc-123/custom-title.json"
            ))
        );
    }

    #[test]
    fn sidecar_path_rejects_non_jsonl() {
        assert_eq!(custom_title_sidecar_path(Path::new("/a/b/notes.txt")), None);
        assert_eq!(custom_title_sidecar_path(Path::new("/a")), None);
    }

    #[test]
    fn inject_requires_idle_margin() {
        let now = 1_000_000;
        let idle = now - RENAME_IDLE_MARGIN_SECS;
        assert!(should_inject_rename_keys("plan", Some(idle), now));
        assert!(!should_inject_rename_keys("plan", Some(idle + 1), now));
    }

    #[test]
    fn inject_fails_closed_without_activity() {
        assert!(!should_inject_rename_keys("plan", None, 1_000_000));
    }

    #[test]
    fn inject_rejects_multi_line_names() {
        let idle = 1_000_000 - RENAME_IDLE_MARGIN_SECS;
        assert!(!should_inject_rename_keys("a\nb", Some(idle), 1_000_000));
        assert!(!should_inject_rename_keys("a\rb", Some(idle), 1_000_000));
        assert!(!should_inject_rename_keys("", Some(idle), 1_000_000));
    }

    #[test]
    fn inject_rejects_control_bytes() {
        // send-keys -l types raw bytes; a raw-mode TUI reads control bytes as
        // keystrokes (Esc opens menus, Ctrl-C interrupts), not as text.
        let idle = 1_000_000 - RENAME_IDLE_MARGIN_SECS;
        for name in [
            "a\u{1b}b", // Esc
            "a\u{3}b",  // Ctrl-C
            "a\u{4}b",  // EOT
            "a\tb",     // Tab
            "a\u{7f}b", // DEL
            "a\u{9b}b", // CSI
        ] {
            assert!(
                !should_inject_rename_keys(name, Some(idle), 1_000_000),
                "{name:?}"
            );
        }
    }

    #[test]
    fn sid_must_be_a_plain_uuid() {
        assert!(is_valid_claude_session_id(
            "55be7663-103d-4f5f-bcaf-f30fc8b0249a"
        ));
        // Traversal and malformed ids must never steer a write path.
        assert!(!is_valid_claude_session_id("../../home/u/x"));
        assert!(!is_valid_claude_session_id("abc/def"));
        assert!(!is_valid_claude_session_id(""));
        assert!(!is_valid_claude_session_id(
            "55be7663-103d-4f5f-bcaf-f30fc8b0249a.jsonl"
        ));
    }

    #[test]
    fn inject_allows_tricky_single_line_names() {
        // Leading `-`, quotes, and spaces are safe: send_cli_keys passes the
        // text after a `--` terminator via `send-keys -l`.
        let idle = 1_000_000 - RENAME_IDLE_MARGIN_SECS;
        assert!(should_inject_rename_keys("--force", Some(idle), 1_000_000));
        assert!(should_inject_rename_keys(
            "my \"plan\"",
            Some(idle),
            1_000_000
        ));
        assert!(should_inject_rename_keys(
            "fix: it's broken",
            Some(idle),
            1_000_000
        ));
    }

    #[test]
    fn sidecar_write_and_transcript_append() {
        let dir = std::env::temp_dir().join(format!("bc-rename-{}", std::process::id()));
        let transcript = dir.join("sess.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();

        write_sidecar(&transcript, "sess", "new name").unwrap();
        let sidecar = custom_title_sidecar_path(&transcript).unwrap();
        let raw = std::fs::read_to_string(&sidecar).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["customTitle"], "new name");
        assert_eq!(raw, "{\"customTitle\":\"new name\"}");

        append_transcript_record(&transcript, "sess", "new name").unwrap();
        let transcript_text = std::fs::read_to_string(&transcript).unwrap();
        let lines: Vec<&str> = transcript_text.lines().collect();
        assert_eq!(lines.len(), 2);
        let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(record["type"], "custom-title");
        assert_eq!(record["customTitle"], "new name");
        assert_eq!(record["sessionId"], "sess");
        // The pre-existing line is untouched.
        assert_eq!(lines[0], "{\"type\":\"user\"}");

        // Private-file convention: sidecar dir 0700, sidecar file 0600. (The
        // test pre-creates the transcript's parent under the temp dir, so the
        // asserted dir is the one write_sidecar itself creates.)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sidecar_dir = sidecar.parent().unwrap();
            let dir_mode = std::fs::metadata(sidecar_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            let file_mode = std::fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sidecar_escaping_is_not_hand_rolled() {
        let dir = std::env::temp_dir().join(format!("bc-rename-esc-{}", std::process::id()));
        let transcript = dir.join("sess.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&transcript, "").unwrap();

        write_sidecar(&transcript, "sess", "a\"b\\c\nd").unwrap();
        let raw = std::fs::read_to_string(custom_title_sidecar_path(&transcript).unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["customTitle"], "a\"b\\c\nd");

        std::fs::remove_dir_all(&dir).ok();
    }
}
