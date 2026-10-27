use std::path::PathBuf;

use api_types::handover::{
    ConfirmHandoverResponse, HandoverPhase, HandoverResponse, HandoverWorkspaceSessionRequest,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use db::models::{
    cli_pane_binding::CliPaneBinding,
    execution_process::ExecutionProcess,
    session::Session,
    session_handover::SessionHandover,
    workspace_spawn_reservation::{SpawnReservationHolder, WorkspaceSpawnReservation},
};
use deployment::Deployment;
use local_deployment::{
    cli_activity::{AgentPresence, probe_workspace_agent},
    pty::{
        CliRespawnClaim, cli_handover_bootstrap, cli_pane_agent_processes,
        cli_tmux_session_exists_checked, respawn_cli_tmux_pane,
    },
};
use serde::{Deserialize, Serialize};
use utils::response::ApiResponse;
use uuid::Uuid;

use super::{
    cli_agent::{cli_context, cli_lifecycle_lock},
    terminal,
};
use crate::{DeploymentImpl, error::ApiError};

#[derive(Serialize, Deserialize)]
struct Launch {
    cwd: PathBuf,
    successor_bootstrap: String,
    fallback_bootstrap: String,
    successor_exit: PathBuf,
    fallback_exit: PathBuf,
}

fn json_error(error: serde_json::Error) -> ApiError {
    ApiError::BadGateway(format!("Invalid durable handover data: {error}"))
}

fn receipt(op: &SessionHandover) -> Result<HandoverResponse, serde_json::Error> {
    Ok(HandoverResponse {
        operation_id: op.operation_id,
        workspace_id: op.workspace_id,
        predecessor_session_id: op.predecessor_session_id,
        successor_session_id: op.successor_session_id,
        successor_native_session_id: op.successor_native_session_id.clone(),
        phase: serde_json::from_value(serde_json::Value::String(op.phase.clone()))?,
        deadline_at: op.deadline_at,
        binding_id: op.binding_id,
        fallback_session_id: op.fallback_session_id.clone(),
        confirmed_at: op.confirmed_at,
        failure: op.failure.clone(),
    })
}

async fn load(deployment: &DeploymentImpl, id: Uuid) -> Result<SessionHandover, ApiError> {
    SessionHandover::find(&deployment.db().pool, id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Handover operation not found".into()))
}

async fn get_operation(
    State(deployment): State<DeploymentImpl>,
    Path(id): Path<Uuid>,
) -> Result<Json<ApiResponse<HandoverResponse>>, ApiError> {
    Ok(Json(ApiResponse::success(
        receipt(&load(&deployment, id).await?).map_err(json_error)?,
    )))
}

async fn begin(
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<HandoverWorkspaceSessionRequest>,
) -> Result<Json<ApiResponse<HandoverResponse>>, ApiError> {
    let _guard = cli_lifecycle_lock(request.workspace_id).lock_owned().await;
    let pool = &deployment.db().pool;
    let request_json = serde_json::to_string(&request).map_err(json_error)?;
    if let Some(op) =
        SessionHandover::find_request(pool, request.workspace_id, &request.request_id).await?
    {
        if op.request_json != request_json {
            return Err(ApiError::Conflict(
                "request_id already names a different handover".into(),
            ));
        }
        return Ok(Json(ApiResponse::success(
            receipt(&op).map_err(json_error)?,
        )));
    }
    let reservation =
        WorkspaceSpawnReservation::acquire(pool, request.workspace_id, SpawnReservationHolder::Cli)
            .await?
            .ok_or_else(|| {
                ApiError::Conflict("Another writer is starting for this workspace".into())
            })?;
    let result = async {
    if request.request_id.is_empty()
        || request.initial_prompt.trim().is_empty()
        || request.confirm_deadline_secs == 0
    {
        return Err(ApiError::BadRequest(
            "request_id, initial_prompt and a positive confirmation deadline are required".into(),
        ));
    }
    Uuid::parse_str(&request.fallback_session_id).map_err(|_| {
        ApiError::BadRequest("fallback_session_id must be a native Claude UUID".into())
    })?;
    if SessionHandover::unfinished(pool)
        .await?
        .iter()
        .any(|op| op.workspace_id == request.workspace_id)
    {
        return Err(ApiError::Conflict(
            "A handover is already awaiting confirmation".into(),
        ));
    }
    let binding = CliPaneBinding::find_active_for_workspace(pool, request.workspace_id)
        .await?
        .filter(|b| {
            b.id == request.expected_binding_id
                && b.claude_session_id.as_deref() == Some(&request.fallback_session_id)
        })
        .ok_or_else(|| {
            ApiError::Conflict("The expected predecessor binding and fallback do not match".into())
        })?;
    let successor = Session::find_by_id(pool, request.successor_session_id)
        .await?
        .filter(|s| {
            s.workspace_id == request.workspace_id
                && s.id != binding.session_id
                && s.executor.as_deref() == Some("CLAUDE_CODE")
        })
        .ok_or_else(|| {
            ApiError::BadRequest(
                "Successor must be a separate Claude session in this workspace".into(),
            )
        })?;
    for session_id in [binding.session_id, successor.id] {
        if ExecutionProcess::has_running_coding_agent_for_session(pool, session_id).await? {
            return Err(ApiError::Conflict(
                "A headless run owns a handover session".into(),
            ));
        }
    }
    if CliPaneBinding::resume_session_id(pool, successor.id)
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(
            "Successor must have no existing native conversation".into(),
        ));
    }
    let context = cli_context(&deployment, request.workspace_id).await?;
    if context.program != "claude" || !cli_tmux_session_exists_checked(request.workspace_id).await?
    {
        return Err(ApiError::Conflict(
            "Handover requires an existing Claude CLI pane".into(),
        ));
    }
    let workspace = db::models::workspace::Workspace::find_by_id(pool, request.workspace_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Workspace not found".into()))?;
    let base_dir = PathBuf::from(
        workspace
            .container_ref
            .ok_or_else(|| ApiError::BadRequest("Workspace has no directory".into()))?,
    );
    let successor_dir = successor
        .effective_working_dir(&base_dir)
        .ok_or_else(|| ApiError::BadRequest("Successor directory unavailable".into()))?;
    if successor_dir != context.working_dir {
        return Err(ApiError::BadRequest(
            "Handover sessions must use the same working directory".into(),
        ));
    }
    let operation_id = Uuid::new_v4();
    let native_id = Uuid::new_v4().to_string();
    let capability = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let dir = utils::assets::asset_dir().join("handovers");
    std::fs::create_dir_all(&dir)?;
    let successor_exit = dir.join(format!("{operation_id}.successor-exit"));
    let fallback_exit = dir.join(format!("{operation_id}.fallback-exit"));
    let mut spec = context.spec.clone();
    spec.session_args
        .extend(["--session-id".into(), native_id.clone()]);
    let prompt = format!(
        "{}\n\nBetterCoding handover operation_id={operation_id}, successor_session_id={}, successor_native_session_id={native_id}. Read get_handover, then confirm_handover before accepting CoC ownership or cleanup.",
        request.initial_prompt, successor.id
    );
    let successor_bootstrap = cli_handover_bootstrap(
        request.workspace_id,
        &spec,
        None,
        Some(&prompt),
        &successor_exit,
        &[
            ("BC_HANDOVER_CAPABILITY", &capability),
            ("BC_HANDOVER_OPERATION_ID", &operation_id.to_string()),
        ],
    )?;
    let fallback_bootstrap = cli_handover_bootstrap(
        request.workspace_id,
        &context.spec,
        Some(&request.fallback_session_id),
        None,
        &fallback_exit,
        &[
            ("BC_HANDOVER_CAPABILITY", &format!("{capability}-fallback")),
            ("BC_HANDOVER_OPERATION_ID", ""),
        ],
    )?;
    let launch = Launch {
        cwd: context.working_dir,
        successor_bootstrap,
        fallback_bootstrap,
        successor_exit,
        fallback_exit,
    };
    let op = SessionHandover {
        operation_id,
        workspace_id: request.workspace_id,
        request_id: request.request_id.clone(),
        request_json,
        predecessor_session_id: binding.session_id,
        successor_session_id: successor.id,
        predecessor_binding_id: binding.id,
        binding_id: Uuid::new_v4(),
        successor_native_session_id: native_id,
        fallback_session_id: request.fallback_session_id.clone(),
        capability,
        phase: "starting".into(),
        deadline_at: Utc::now() + Duration::seconds(i64::from(request.confirm_deadline_secs)),
        confirmed_at: None,
        failure: None,
        rollback_started: false,
        fallback_started_at: None,
        fallback_ready_at: None,
        fallback_pid: None,
        fallback_start_time: None,
        launch_json: serde_json::to_string(&launch).map_err(json_error)?,
    };
        op.insert(pool).await?;
        match respawn(
            &deployment,
            &op,
            &launch.successor_bootstrap,
            &launch.cwd,
            &launch.successor_exit,
        )
        .await
        {
            Ok(()) => op.set_phase(pool, "awaiting_confirmation", None).await?,
            Err(error) => {
                rollback(
                    &deployment,
                    &op,
                    &format!("Successor launch failed: {error}"),
                )
                .await?
            }
        }
        Ok(Json(ApiResponse::success(
            receipt(&load(&deployment, operation_id).await?).map_err(json_error)?,
        )))
    }
    .await;
    WorkspaceSpawnReservation::release(pool, request.workspace_id, &reservation.fence).await?;
    result
}

async fn native_agent_alive(workspace_id: Uuid, flag: &str, native_id: &str) -> bool {
    cli_pane_agent_processes(workspace_id, &["claude"])
        .await
        .is_some_and(|(_, processes)| {
            processes.iter().any(|process| {
                process.argv.as_ref().is_some_and(|argv| {
                    argv.windows(2)
                        .any(|pair| pair[0] == flag && pair[1] == native_id)
                })
            })
        })
}

async fn respawn(
    deployment: &DeploymentImpl,
    op: &SessionHandover,
    bootstrap: &str,
    cwd: &std::path::Path,
    exit_file: &std::path::Path,
) -> Result<(), ApiError> {
    let claim = CliRespawnClaim::try_claim(op.workspace_id)
        .ok_or_else(|| ApiError::Conflict("A CLI respawn is in progress".into()))?;
    respawn_cli_tmux_pane(op.workspace_id, &claim, "claude", cwd, bootstrap)
        .await
        .map_err(|e| ApiError::BadGateway(e.to_string()))?;
    let fallback = exit_file
        != serde_json::from_str::<Launch>(&op.launch_json)
            .map_err(json_error)?
            .successor_exit;
    let (flag, native_id) = if fallback {
        ("--resume", op.fallback_session_id.as_str())
    } else {
        ("--session-id", op.successor_native_session_id.as_str())
    };
    let mut ready = false;
    for _ in 0..30 {
        if exit_file.exists() {
            break;
        }
        if native_agent_alive(op.workspace_id, flag, native_id).await {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    if !ready {
        return Err(ApiError::BadGateway(
            "Claude failed to start in the handover pane".into(),
        ));
    }
    terminal::forget_ended_session(&deployment.db().pool, op.workspace_id).await;
    Ok(())
}

async fn rollback(
    deployment: &DeploymentImpl,
    op: &SessionHandover,
    reason: &str,
) -> Result<(), ApiError> {
    let pool = &deployment.db().pool;
    if CliPaneBinding::find_active_for_workspace(pool, op.workspace_id)
        .await?
        .map(|b| b.id)
        != Some(op.binding_id)
    {
        op.set_phase(
            pool,
            "failed",
            Some("Handover binding was superseded; newer pane preserved"),
        )
        .await?;
        return Ok(());
    }
    for session_id in [op.predecessor_session_id, op.successor_session_id] {
        if ExecutionProcess::has_running_coding_agent_for_session(pool, session_id).await? {
            op.set_phase(
                pool,
                "failed",
                Some("Fallback refused: a headless writer owns a handover session"),
            )
            .await?;
            return Ok(());
        }
    }
    let launch: Launch = serde_json::from_str(&op.launch_json).map_err(json_error)?;
    if op.fallback_started_at.is_some() {
        if fallback_process_identity(op).await.is_some()
            || op.fallback_pid.is_some()
            || launch.fallback_exit.exists()
        {
            return finish_fallback(deployment, op, Utc::now()).await;
        }
        let request: HandoverWorkspaceSessionRequest =
            serde_json::from_str(&op.request_json).map_err(json_error)?;
        if op.fallback_started_at.is_some_and(|at| {
            Utc::now() >= at + Duration::seconds(i64::from(request.confirm_deadline_secs))
        }) {
            return finish_fallback(deployment, op, Utc::now()).await;
        }
        if probe_workspace_agent(op.workspace_id, "claude").await == AgentPresence::Unknown {
            return Ok(());
        }
    } else {
        sqlx::query("UPDATE session_handovers SET rollback_started = 1, fallback_started_at = ?, failure = ? WHERE operation_id = ?")
            .bind(Utc::now()).bind(reason).bind(op.operation_id).execute(pool).await?;
    }
    match respawn(
        deployment,
        op,
        &launch.fallback_bootstrap,
        &launch.cwd,
        &launch.fallback_exit,
    )
    .await
    {
        Ok(()) => {
            if let Some((pid, start)) = fallback_process_identity(op).await {
                sqlx::query("UPDATE session_handovers SET fallback_pid = ?, fallback_start_time = ? WHERE operation_id = ?")
                    .bind(i64::from(pid)).bind(start).bind(op.operation_id).execute(pool).await?;
            }
        }
        Err(error) => {
            op.set_phase(
                pool,
                "failed",
                Some(&format!("{reason}; fallback failed: {error}")),
            )
            .await?
        }
    }
    Ok(())
}

async fn finish_fallback(
    deployment: &DeploymentImpl,
    op: &SessionHandover,
    now: DateTime<Utc>,
) -> Result<(), ApiError> {
    let launch: Launch = serde_json::from_str(&op.launch_json).map_err(json_error)?;
    let identity = fallback_process_identity(op).await;
    if launch.fallback_exit.exists() {
        op.set_phase(
            &deployment.db().pool,
            "failed",
            Some("Fallback failed during initialization: process exited"),
        )
        .await?;
    } else if op.fallback_ready_at.is_some()
        && identity.as_ref().is_some_and(|(pid, start)| {
            op.fallback_pid == Some(i64::from(*pid))
                && op.fallback_start_time.as_ref() == Some(start)
        })
    {
        let pool = &deployment.db().pool;
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT session_id FROM claude_session_links WHERE claude_session_id = ?",
        )
        .bind(&op.fallback_session_id)
        .fetch_optional(&mut *tx)
        .await?;
        if owner.is_some_and(|owner| owner != op.predecessor_session_id) {
            drop(tx);
            op.set_phase(
                pool,
                "failed",
                Some("Fallback ownership changed; current transcript owner preserved"),
            )
            .await?;
            return Ok(());
        }
        sqlx::query("INSERT INTO claude_session_links (claude_session_id, session_id, workspace_id, cwd, bound_via) VALUES (?, ?, ?, ?, 'cli-resume') ON CONFLICT(claude_session_id) DO NOTHING")
            .bind(&op.fallback_session_id).bind(op.predecessor_session_id).bind(op.workspace_id).bind(launch.cwd.to_string_lossy().as_ref()).execute(&mut *tx).await?;
        let changed=sqlx::query("UPDATE cli_pane_bindings SET session_id = ?, claude_session_id = ?, bound_via = 'cli-resume' WHERE id = ? AND released_at IS NULL")
            .bind(op.predecessor_session_id).bind(&op.fallback_session_id).bind(op.binding_id).execute(&mut *tx).await?.rows_affected();
        if changed == 1 {
            sqlx::query("UPDATE session_handovers SET phase = 'rolled_back' WHERE operation_id = ? AND phase IN ('starting','awaiting_confirmation')")
                .bind(op.operation_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
    } else {
        let request: HandoverWorkspaceSessionRequest =
            serde_json::from_str(&op.request_json).map_err(json_error)?;
        if op.fallback_started_at.is_some_and(|at| {
            now >= at + Duration::seconds(i64::from(request.confirm_deadline_secs))
        }) {
            op.set_phase(&deployment.db().pool, "failed", Some("Fallback needs recovery: initialization receipt deadline expired; process preserved")).await?;
        }
    }
    Ok(())
}

async fn fallback_process_identity(op: &SessionHandover) -> Option<(u32, String)> {
    let (_, processes) = cli_pane_agent_processes(op.workspace_id, &["claude"]).await?;
    let capability = format!("BC_HANDOVER_CAPABILITY={}-fallback", op.capability);
    for process in processes {
        if !process.argv.as_ref().is_some_and(|argv| {
            argv.windows(2)
                .any(|pair| pair[0] == "--resume" && pair[1] == op.fallback_session_id)
        }) {
            continue;
        }
        let start = process_start_identity(process.pid)?;
        let env = std::fs::read(format!("/proc/{}/environ", process.pid)).ok()?;
        if env
            .split(|b| *b == 0)
            .any(|entry| entry == capability.as_bytes())
            && process_start_identity(process.pid).as_ref() == Some(&start)
        {
            return Some((process.pid, start));
        }
    }
    None
}

pub(super) fn process_start_identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(
        stat.rsplit_once(") ")?
            .1
            .split_whitespace()
            .nth(19)?
            .to_string(),
    )
}

pub(super) async fn fallback_ready(
    deployment: &DeploymentImpl,
    workspace_id: Uuid,
    native_id: &str,
    capability: &str,
) -> Result<(), ApiError> {
    let Some((Some("claude"), processes)) =
        cli_pane_agent_processes(workspace_id, &["claude"]).await
    else {
        return Ok(());
    };
    for process in processes {
        if process.argv.as_ref().is_some_and(|argv| {
            argv.windows(2)
                .any(|pair| pair[0] == "--resume" && pair[1] == native_id)
        }) && let Some(start) = process_start_identity(process.pid)
        {
            SessionHandover::record_fallback_ready(
                &deployment.db().pool,
                workspace_id,
                native_id,
                capability,
                process.pid,
                &start,
            )
            .await?;
        }
    }
    Ok(())
}

async fn confirm(
    State(deployment): State<DeploymentImpl>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<ConfirmHandoverResponse>>, ApiError> {
    let op = load(&deployment, id).await?;
    let _guard = cli_lifecycle_lock(op.workspace_id).lock_owned().await;
    let op = load(&deployment, id).await?;
    let token = headers
        .get("x-bc-handover-capability")
        .and_then(|s| s.to_str().ok());
    if token != Some(op.capability.as_str()) {
        return Err(ApiError::Conflict(
            "Confirmation requires the launched successor capability".into(),
        ));
    }
    if CliPaneBinding::find_active_for_workspace(&deployment.db().pool, op.workspace_id)
        .await?
        .map(|b| b.id)
        != Some(op.binding_id)
    {
        return Err(ApiError::Conflict(
            "The handover binding has been superseded".into(),
        ));
    }
    if op.phase != "confirmed" {
        let launch: Launch = serde_json::from_str(&op.launch_json).map_err(json_error)?;
        if op.phase != "awaiting_confirmation"
            || op.rollback_started
            || Utc::now() >= op.deadline_at
            || launch.successor_exit.exists()
            || !native_agent_alive(
                op.workspace_id,
                "--session-id",
                &op.successor_native_session_id,
            )
            .await
        {
            return Err(ApiError::Conflict(
                "Handover is not eligible for confirmation".into(),
            ));
        }
        sqlx::query("UPDATE session_handovers SET phase = 'confirmed', confirmed_at = ? WHERE operation_id = ?")
            .bind(Utc::now()).bind(id).execute(&deployment.db().pool).await?;
    }
    let op = load(&deployment, id).await?;
    Ok(Json(ApiResponse::success(ConfirmHandoverResponse {
        operation_id: id,
        phase: HandoverPhase::Confirmed,
        confirmed_at: op
            .confirmed_at
            .ok_or_else(|| ApiError::Conflict("Confirmation timestamp missing".into()))?,
        successor_session_id: op.successor_session_id,
        successor_native_session_id: op.successor_native_session_id,
        binding_id: op.binding_id,
    })))
}

/// The same pass runs on startup and each monitor tick; wall time remains durable across restarts.
pub async fn reconcile(deployment: &DeploymentImpl, now: DateTime<Utc>) -> Result<(), ApiError> {
    for pending in SessionHandover::unfinished(&deployment.db().pool).await? {
        let _guard = cli_lifecycle_lock(pending.workspace_id).lock_owned().await;
        let op = load(deployment, pending.operation_id).await?;
        if !matches!(op.phase.as_str(), "starting" | "awaiting_confirmation") {
            continue;
        }
        let launch: Launch = serde_json::from_str(&op.launch_json).map_err(json_error)?;
        let stale =
            CliPaneBinding::find_active_for_workspace(&deployment.db().pool, op.workspace_id)
                .await?
                .map(|b| b.id)
                != Some(op.binding_id);
        if stale {
            op.set_phase(
                &deployment.db().pool,
                "failed",
                Some("Handover binding was superseded; newer pane preserved"),
            )
            .await?;
            continue;
        }
        if op.fallback_started_at.is_some()
            && (fallback_process_identity(&op).await.is_some()
                || op.fallback_pid.is_some()
                || launch.fallback_exit.exists())
        {
            finish_fallback(deployment, &op, now).await?;
            continue;
        }
        let dead = matches!(
            probe_workspace_agent(op.workspace_id, "claude").await,
            AgentPresence::Absent | AgentPresence::NoSession
        );
        if op.phase == "starting"
            || op.rollback_started
            || now >= op.deadline_at
            || launch.successor_exit.exists()
            || dead
        {
            let Some(reservation) = WorkspaceSpawnReservation::acquire(
                &deployment.db().pool,
                op.workspace_id,
                SpawnReservationHolder::Cli,
            )
            .await?
            else {
                continue;
            };
            let result = rollback(
                deployment,
                &op,
                "Successor exited, launch was interrupted, or confirmation deadline passed",
            )
            .await;
            WorkspaceSpawnReservation::release(
                &deployment.db().pool,
                op.workspace_id,
                &reservation.fence,
            )
            .await?;
            result?;
        }
    }
    Ok(())
}

pub fn start_monitor(deployment: DeploymentImpl, shutdown: tokio_util::sync::CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => if let Err(error) = reconcile(&deployment, Utc::now()).await { tracing::error!(%error, "handover reconciliation failed"); }
            }
        }
    });
}

pub(super) fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route("/handovers", post(begin))
        .route("/handovers/{operation_id}", get(get_operation))
        .route("/handovers/{operation_id}/confirm", post(confirm))
}
