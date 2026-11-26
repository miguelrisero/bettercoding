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
            || is_claude_temp_tasks_dir(path, uid, &cwd, &request.native_session_id)
    };
    // An open session-specific transcript or task directory ties a CLI process
    // to its native UUID. A `claude attach <short>` pane client holds neither:
    // Claude Code's job record for `<short>` must name this session and
    // directory, and exactly one other process of this user in the directory
    // must hold the proof. Its start time is read before its descriptors, so a
    // reused PID fails the final start-time check.
    let (owner, owner_start) = match attach_prefix(processes[0].argv.as_deref().unwrap_or_default())
    {
        Some(short) => {
            let home = dirs::home_dir().ok_or_else(conflict)?;
            if !job_names_session(&home, short, &request.native_session_id, &cwd) {
                return Err(conflict());
            }
            attached_owner(request.expected_pid, uid, &cwd, &is_proof).ok_or_else(conflict)?
        }
        None => (request.expected_pid, request.expected_start_time),
    };
    if std::fs::metadata(format!("/proc/{owner}"))?.uid() != uid || !holds_proof(owner, &is_proof) {
        return Err(conflict());
    }
    let live = cli_pane_agent_processes(workspace.id, CLI_AGENT_PROGRAMS).await;
    if process_start(request.expected_pid) != Some(request.expected_start_time)
        || cli_workspace_pane_id(workspace.id).await? != request.expected_pane_id
        || !live.is_some_and(|(program, processes)| {
            program == Some("claude")
                && processes.len() == 1
                && processes[0].pid == request.expected_pid
        })
        || std::fs::read_link(format!("/proc/{}/cwd", request.expected_pid))?.canonicalize()? != cwd
        || std::fs::read_link(format!("/proc/{owner}/cwd"))?.canonicalize()? != cwd
        || !holds_proof(owner, &is_proof)
        || process_start(owner) != Some(owner_start)
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

/// The session prefix when `argv` is `claude attach <prefix>`.
#[cfg(target_os = "linux")]
fn attach_prefix(argv: &[String]) -> Option<&str> {
    let [program, command, prefix] = argv else {
        return None;
    };
    (command == "attach" && std::path::Path::new(program).file_name()? == "claude")
        .then_some(prefix.as_str())
}

/// Whether `path` is Claude Code's task directory for `native` in `cwd`:
/// `<tmp>/claude-<uid>/<cwd with every non-alphanumeric byte as '-'>/<native>/tasks`,
/// a directory inside a `claude-<uid>` directory this user owns.
#[cfg(target_os = "linux")]
fn is_claude_temp_tasks_dir(
    path: &std::path::Path,
    uid: u32,
    cwd: &std::path::Path,
    native: &str,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    let project: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut up = path.ancestors().skip(1);
    path.file_name() == Some("tasks".as_ref())
        && up.next().and_then(std::path::Path::file_name) == Some(native.as_ref())
        && up.next().and_then(std::path::Path::file_name) == Some(project.as_ref())
        && up.next().is_some_and(|root| {
            root.file_name() == Some(format!("claude-{uid}").as_ref())
                && std::fs::symlink_metadata(root).is_ok_and(|m| m.is_dir() && m.uid() == uid)
        })
        && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.uid() == uid)
}

/// Whether Claude Code's job record `~/.claude/jobs/<short>/state.json`
/// (what `claude attach <short>` resolves) names `native` in `cwd`.
#[cfg(target_os = "linux")]
fn job_names_session(
    home: &std::path::Path,
    short: &str,
    native: &str,
    cwd: &std::path::Path,
) -> bool {
    if short.len() != 8 || !short.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let Ok(raw) = std::fs::read(home.join(".claude/jobs").join(short).join("state.json")) else {
        return false;
    };
    let Ok(job) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    job["daemonShort"] == short
        && job["sessionId"] == native
        && job["cwd"]
            .as_str()
            .and_then(|dir| std::fs::canonicalize(dir).ok())
            .is_some_and(|dir| dir == cwd)
}

#[cfg(target_os = "linux")]
fn open_paths(pid: u32) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|fds| {
            fds.flatten()
                .filter_map(|fd| std::fs::read_link(fd.path()).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn holds_proof(pid: u32, is_proof: &impl Fn(&std::path::Path) -> bool) -> bool {
    open_paths(pid).iter().any(|path| is_proof(path))
}

/// The one process of `uid`, other than the attach `client`, that runs in
/// `cwd` and holds the session proof open, with its start time read before
/// its descriptors. `None` when none or several do.
#[cfg(target_os = "linux")]
fn attached_owner(
    client: u32,
    uid: u32,
    cwd: &std::path::Path,
    is_proof: &impl Fn(&std::path::Path) -> bool,
) -> Option<(u32, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut owners = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != client)
        .filter_map(|pid| Some((pid, process_start(pid)?)))
        .filter(|&(pid, _)| {
            std::fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == uid)
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
    fn attach_prefix_reads_only_a_claude_attach() {
        assert_eq!(
            attach_prefix(&argv(&["claude", "attach", "356524b6"])),
            Some("356524b6")
        );
        assert_eq!(
            attach_prefix(&argv(&["/usr/bin/claude", "attach", "356524b6"])),
            Some("356524b6")
        );
        assert_eq!(attach_prefix(&argv(&["claude", "--resume", NATIVE])), None);
        assert_eq!(attach_prefix(&argv(&["node", "attach", "356524b6"])), None);
        assert_eq!(attach_prefix(&argv(&["claude"])), None);
    }

    #[test]
    fn temp_tasks_dir_must_be_this_session_in_this_project() {
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let base = tempfile::tempdir().unwrap();
        let cwd = std::path::Path::new("/home/u/.repos/app");
        let tasks = base
            .path()
            .join(format!("claude-{uid}/-home-u--repos-app/{NATIVE}/tasks"));
        std::fs::create_dir_all(&tasks).unwrap();
        assert!(is_claude_temp_tasks_dir(&tasks, uid, cwd, NATIVE));
        assert!(!is_claude_temp_tasks_dir(
            &tasks,
            uid,
            std::path::Path::new("/home/u/other"),
            NATIVE
        ));
        assert!(!is_claude_temp_tasks_dir(
            &tasks,
            uid,
            cwd,
            "19f21039-8fb4-42c7-a85e-6805d697cb7a"
        ));
        assert!(!is_claude_temp_tasks_dir(
            &tasks.join("x.output"),
            uid,
            cwd,
            NATIVE
        ));

        // A root named for another uid but owned by this one.
        let other = base.path().join(format!(
            "claude-{}/-home-u--repos-app/{NATIVE}/tasks",
            uid + 1
        ));
        std::fs::create_dir_all(&other).unwrap();
        assert!(!is_claude_temp_tasks_dir(&other, uid + 1, cwd, NATIVE));

        // A regular file named `tasks`.
        let file = base
            .path()
            .join(format!("claude-{uid}/-home-u--repos-app/x/tasks"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"").unwrap();
        assert!(!is_claude_temp_tasks_dir(&file, uid, cwd, "x"));
    }

    #[test]
    fn job_record_must_name_this_session_and_directory() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().canonicalize().unwrap();
        let job = home.path().join(".claude/jobs/356524b6");
        std::fs::create_dir_all(&job).unwrap();
        let record = serde_json::json!({
            "daemonShort": "356524b6",
            "sessionId": NATIVE,
            "cwd": cwd.to_string_lossy(),
        });
        std::fs::write(job.join("state.json"), record.to_string()).unwrap();

        assert!(job_names_session(home.path(), "356524b6", NATIVE, &cwd));
        assert!(!job_names_session(
            home.path(),
            "356524b6",
            "356524b6-0000-4000-8000-000000000000",
            &cwd
        ));
        assert!(!job_names_session(
            home.path(),
            "356524b6",
            NATIVE,
            std::path::Path::new("/")
        ));
        assert!(!job_names_session(home.path(), "19f21039", NATIVE, &cwd));
        assert!(!job_names_session(home.path(), "../jobs", NATIVE, &cwd));
    }

    #[test]
    fn attached_owner_finds_the_single_process_holding_the_proof() {
        let base = tempfile::tempdir().unwrap();
        let proof = base.path().join("proof");
        std::fs::create_dir(&proof).unwrap();
        let _held = std::fs::File::open(&proof).unwrap();
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let is_proof = |path: &std::path::Path| path == proof;
        let me = std::process::id();

        assert_eq!(
            attached_owner(0, uid, &cwd, &is_proof),
            Some((me, process_start(me).unwrap()))
        );
        assert_eq!(attached_owner(me, uid, &cwd, &is_proof), None);
        assert_eq!(attached_owner(0, uid + 1, &cwd, &is_proof), None);
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
