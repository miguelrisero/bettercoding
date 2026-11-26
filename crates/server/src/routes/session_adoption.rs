use api_types::handover::{AdoptWorkspaceSessionRequest, AdoptWorkspaceSessionResponse};
use axum::{Json, Router, extract::State, routing::post};
use db::models::{
    cli_native_file::CliNativeFile, cli_pane_binding::CliPaneBinding, session::Session,
    workspace::Workspace,
};
use deployment::Deployment;
use local_deployment::pty::{CLI_AGENT_PROGRAMS, cli_pane_agent_processes, cli_workspace_pane_id};
use utils::response::ApiResponse;

use super::cli_agent::cli_lifecycle_lock;
use crate::{DeploymentImpl, error::ApiError};

fn conflict() -> ApiError {
    ApiError::Conflict("Adoption identity, binding or transcript evidence does not match".into())
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
async fn adopt(
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<AdoptWorkspaceSessionRequest>,
) -> Result<Json<ApiResponse<AdoptWorkspaceSessionResponse>>, ApiError> {
    let _guard = cli_lifecycle_lock(request.workspace_id).lock_owned().await;
    let pool = &deployment.db().pool;
    let reservation = db::models::workspace_spawn_reservation::WorkspaceSpawnReservation::acquire(
        pool,
        request.workspace_id,
        db::models::workspace_spawn_reservation::SpawnReservationHolder::Cli,
    )
    .await?
    .ok_or_else(conflict)?;
    let result = adopt_checked(&deployment, &request).await;
    db::models::workspace_spawn_reservation::WorkspaceSpawnReservation::release(
        pool,
        request.workspace_id,
        &reservation.fence,
    )
    .await?;
    result
}

#[cfg(target_os = "linux")]
async fn adopt_checked(
    deployment: &DeploymentImpl,
    request: &AdoptWorkspaceSessionRequest,
) -> Result<Json<ApiResponse<AdoptWorkspaceSessionResponse>>, ApiError> {
    use std::os::unix::fs::MetadataExt;
    let pool = &deployment.db().pool;
    let native = uuid::Uuid::parse_str(&request.native_session_id).map_err(|_| conflict())?;
    if native.get_version_num() != 4
        || process_start(request.expected_pid) != Some(request.expected_start_time)
    {
        return Err(conflict());
    }
    let workspace = Workspace::find_by_id(pool, request.workspace_id)
        .await?
        .ok_or_else(conflict)?;
    let session = Session::find_by_id(pool, request.session_id)
        .await?
        .filter(|s| s.workspace_id == workspace.id && s.executor.as_deref() == Some("CLAUDE_CODE"))
        .ok_or_else(conflict)?;
    let cwd = session
        .effective_working_dir(std::path::Path::new(
            workspace.container_ref.as_deref().ok_or_else(conflict)?,
        ))
        .ok_or_else(conflict)?
        .canonicalize()?;
    if std::fs::read_link(format!("/proc/{}/cwd", request.expected_pid))?.canonicalize()? != cwd {
        return Err(conflict());
    }
    if cli_workspace_pane_id(workspace.id).await? != request.expected_pane_id {
        return Err(conflict());
    }
    let (program, processes) = cli_pane_agent_processes(workspace.id, CLI_AGENT_PROGRAMS)
        .await
        .ok_or_else(conflict)?;
    if program != Some("claude") || processes.len() != 1 || processes[0].pid != request.expected_pid
    {
        return Err(conflict());
    }
    let files = CliNativeFile::list_latest_by_sid(pool, &request.native_session_id).await?;
    let local: Vec<_> = files
        .iter()
        .filter(|f| f.discovered_workspace_id == Some(workspace.id))
        .collect();
    if local.is_empty()
        || files.iter().any(|f| {
            f.discovered_workspace_id
                .is_some_and(|id| id != workspace.id)
        })
    {
        return Err(conflict());
    }
    let transcript_paths: Vec<_> = local
        .iter()
        .filter_map(|file| {
            let path = std::path::Path::new(&file.dir_path).join(&file.file_name);
            let metadata = std::fs::metadata(&path).ok()?;
            (metadata.dev() as i64 == file.dev
                && metadata.ino() as i64 == file.inode
                && file.file_name == format!("{}.jsonl", request.native_session_id))
            .then_some(path)
        })
        .collect();
    if transcript_paths.is_empty() {
        return Err(conflict());
    }
    let tasks = dirs::home_dir()
        .ok_or_else(conflict)?
        .join(".claude/tasks")
        .join(&request.native_session_id);
    let uid = std::fs::metadata("/proc/self")?.uid();
    let is_proof = |path: &std::path::Path| {
        transcript_paths.iter().any(|p| p == path)
            || path == tasks
            || is_claude_temp_tasks_dir(path, &request.native_session_id, uid)
    };
    // An open session-specific transcript or task directory ties a CLI process
    // to its native UUID. A `claude attach <prefix>` pane client holds neither:
    // the attached session's own process must, alone among this user's
    // processes in the same directory.
    let owner = match attach_prefix(
        processes[0].argv.as_deref().unwrap_or_default(),
        &request.native_session_id,
    ) {
        Some(true) => {
            attached_owner(request.expected_pid, uid, &cwd, &is_proof).ok_or_else(conflict)?
        }
        Some(false) => return Err(conflict()),
        None => request.expected_pid,
    };
    let owner_start = process_start(owner).ok_or_else(conflict)?;
    if !holds_proof(owner, &is_proof) {
        return Err(conflict());
    }
    let live = cli_pane_agent_processes(workspace.id, CLI_AGENT_PROGRAMS).await;
    if process_start(owner) != Some(owner_start)
        || std::fs::read_link(format!("/proc/{owner}/cwd"))?.canonicalize()? != cwd
        || process_start(request.expected_pid) != Some(request.expected_start_time)
        || cli_workspace_pane_id(workspace.id).await? != request.expected_pane_id
        || !live.is_some_and(|(program, processes)| {
            program == Some("claude")
                && processes.len() == 1
                && processes[0].pid == request.expected_pid
        })
        || std::fs::read_link(format!("/proc/{}/cwd", request.expected_pid))?.canonicalize()? != cwd
    {
        return Err(conflict());
    }
    let adopted = CliPaneBinding::adopt_session(
        pool,
        request.expected_binding_id,
        request.expected_session_id,
        request.expected_native_session_id.as_deref(),
        &request.native_session_id,
        request.session_id,
        request.workspace_id,
        &cwd.to_string_lossy(),
    )
    .await?;
    if adopted.is_none() {
        return Err(conflict());
    }
    let binding = CliPaneBinding::find_active_for_workspace(pool, workspace.id)
        .await?
        .ok_or_else(conflict)?;
    Ok(Json(ApiResponse::success(AdoptWorkspaceSessionResponse {
        workspace_id: workspace.id,
        binding_id: binding.id,
        session_id: request.session_id,
        native_session_id: request.native_session_id.clone(),
        pane_id: request.expected_pane_id.clone(),
        pid: request.expected_pid,
        start_time: request.expected_start_time,
    })))
}

/// `Some(true)` when `argv` is `claude attach <prefix>` for `native` (prefix of
/// at least 8 characters), `Some(false)` for an attach to another session, and
/// `None` when the pane process is no attach client.
#[cfg(target_os = "linux")]
fn attach_prefix(argv: &[String], native: &str) -> Option<bool> {
    let [program, command, prefix] = argv else {
        return None;
    };
    (command == "attach" && std::path::Path::new(program).file_name()? == "claude")
        .then(|| prefix.len() >= 8 && native.starts_with(prefix.as_str()))
}

/// Claude Code's per-session task directory,
/// `<tmp>/claude-<uid>/<project>/<native>/tasks`, inside a `claude-<uid>`
/// directory that this user owns.
#[cfg(target_os = "linux")]
fn is_claude_temp_tasks_dir(path: &std::path::Path, native: &str, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut up = path.ancestors().skip(1);
    path.file_name() == Some("tasks".as_ref())
        && up.next().and_then(std::path::Path::file_name) == Some(native.as_ref())
        && up.next().is_some()
        && up.next().is_some_and(|root| {
            root.file_name() == Some(format!("claude-{uid}").as_ref())
                && std::fs::symlink_metadata(root).is_ok_and(|m| m.is_dir() && m.uid() == uid)
        })
}

#[cfg(target_os = "linux")]
fn holds_proof(pid: u32, is_proof: &impl Fn(&std::path::Path) -> bool) -> bool {
    std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|fds| {
        fds.flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|path| is_proof(&path)))
    })
}

/// The one process of `uid`, other than the attach `client`, that runs in
/// `cwd` and holds the session proof open. `None` when there is none or more
/// than one.
#[cfg(target_os = "linux")]
fn attached_owner(
    client: u32,
    uid: u32,
    cwd: &std::path::Path,
    is_proof: &impl Fn(&std::path::Path) -> bool,
) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let mut owners = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            pid != client
                && std::fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == uid)
                && std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .and_then(|path| path.canonicalize())
                    .is_ok_and(|path| path == cwd)
                && holds_proof(pid, is_proof)
        });
    let owner = owners.next()?;
    owners.next().is_none().then_some(owner)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    const NATIVE: &str = "356524b6-b57a-4bcd-b8ac-b4286bd03194";

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn attach_prefix_matches_only_a_claude_attach_to_this_session() {
        assert_eq!(
            attach_prefix(&argv(&["claude", "attach", "356524b6"]), NATIVE),
            Some(true)
        );
        assert_eq!(
            attach_prefix(&argv(&["/usr/bin/claude", "attach", NATIVE]), NATIVE),
            Some(true)
        );
        assert_eq!(
            attach_prefix(&argv(&["claude", "attach", "19f21039"]), NATIVE),
            Some(false)
        );
        assert_eq!(
            attach_prefix(&argv(&["claude", "attach", "3565"]), NATIVE),
            Some(false)
        );
        assert_eq!(
            attach_prefix(&argv(&["claude", "--resume", NATIVE]), NATIVE),
            None
        );
        assert_eq!(
            attach_prefix(&argv(&["node", "attach", "356524b6"]), NATIVE),
            None
        );
        assert_eq!(attach_prefix(&argv(&["claude"]), NATIVE), None);
    }

    #[test]
    fn temp_tasks_dir_needs_this_session_and_an_owned_claude_uid_root() {
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let base = std::env::temp_dir().join(format!("bc-adopt-test-{}", std::process::id()));
        let root = base.join(format!("claude-{uid}"));
        let tasks = root.join("-home-u-repo").join(NATIVE).join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();

        assert!(is_claude_temp_tasks_dir(&tasks, NATIVE, uid));
        assert!(!is_claude_temp_tasks_dir(
            &tasks,
            "19f21039-8fb4-42c7-a85e-6805d697cb7a",
            uid
        ));
        assert!(!is_claude_temp_tasks_dir(&tasks, NATIVE, uid + 1));
        assert!(!is_claude_temp_tasks_dir(
            &tasks.join("x.output"),
            NATIVE,
            uid
        ));
        let unowned = base.join("claude-0").join("p").join(NATIVE).join("tasks");
        assert!(!is_claude_temp_tasks_dir(&unowned, NATIVE, 0));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn attached_owner_finds_the_single_process_holding_the_proof() {
        let base = std::env::temp_dir().join(format!("bc-adopt-owner-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let proof = base.join("proof");
        let _held = std::fs::File::create(&proof).unwrap();
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let is_proof = |path: &std::path::Path| path == proof;

        assert_eq!(
            attached_owner(0, uid, &cwd, &is_proof),
            Some(std::process::id())
        );
        assert_eq!(
            attached_owner(std::process::id(), uid, &cwd, &is_proof),
            None
        );
        assert_eq!(attached_owner(0, uid + 1, &cwd, &is_proof), None);

        std::fs::remove_dir_all(&base).unwrap();
    }
}

#[cfg(not(target_os = "linux"))]
async fn adopt() -> Result<(), ApiError> {
    Err(ApiError::BadRequest(
        "Session adoption requires Linux process identity evidence".into(),
    ))
}

pub(super) fn router() -> Router<DeploymentImpl> {
    Router::new().route("/sessions/adopt", post(adopt))
}
