use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use api_types::{
    CliScreenResponse, LaunchCliAgentResponse, SendCliTextRequest, SendCliTextResponse,
};
use axum::{
    Router,
    extract::{Query, State, ws::Message},
    response::{IntoResponse, Json},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use db::models::{
    claude_session_link::{ClaudeSessionBoundVia, ClaudeSessionLink},
    cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
    execution_process::ExecutionProcess,
    session::Session,
    workspace::Workspace,
    workspace_cli_activity::{
        CODEX_HOOK_EVENTS, CODEX_ONLY_HOOK_EVENTS, HOOK_EVENTS, WorkspaceCliActivity,
    },
    workspace_repo::WorkspaceRepo,
    workspace_spawn_reservation::{SpawnReservationHolder, WorkspaceSpawnReservation},
};
use deployment::Deployment;
use executors::{
    actions::ExecutorActionType,
    executors::{BaseCodingAgent, StandardCodingAgentExecutor, cli::CliLaunchSpec},
    profile::{ExecutorConfig, ExecutorConfigs},
};
use local_deployment::pty::{
    CLI_AGENT_PROGRAMS, CLI_PROMPT_PARKED_NOTICE, CliPromptDelivery, CliPromptRouting,
    CliSendResult, PtyCommand, capture_cli_pane, cli_pane_agent_running_at, cli_prompt_file_exists,
    cli_tmux_available, cli_tmux_session_exists_checked, cli_tmux_session_name,
    cli_tmux_target_exists, kill_cli_tmux_session_admitted, locate_cli_tmux_target,
    remove_cli_prompt_file, remove_cli_resume_ready_file, route_followup_prompt,
    route_initial_prompt, send_cli_keys_to_live_agent,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{
    DeploymentImpl,
    error::ApiError,
    middleware::signed_ws::{MaybeSignedWebSocket, SignedWsUpgrade},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalMode {
    /// Plain interactive shell (default side terminal).
    #[default]
    Shell,
    /// Persistent tmux-backed session running the interactive `claude` CLI.
    Cli,
}

#[derive(Debug, Deserialize)]
struct TerminalQuery {
    pub workspace_id: Uuid,
    /// `None` (fell back to 80x24) is distinguishable from an explicit value
    /// so the stray-newline regression tripwire below can warn on true
    /// absence without false-positiving on a pane that genuinely measures
    /// 80x24.
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    mode: TerminalMode,
    /// BetterCoding session whose claude conversation CLI mode should resume,
    /// so the terminal joins the exact chat the UI is showing (handover).
    #[serde(default)]
    session_id: Option<Uuid>,
    /// Connect-time browser visibility. A hidden tmux client must be excluded
    /// from shared sizing before its first WebSocket message can arrive.
    #[serde(default)]
    hidden: bool,
}

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalCommand {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    Presence { visible: bool },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalMessage {
    Output { data: String },
    Error { message: String },
    InputRejected { message: String },
}

/// Recover the model + reasoning effort the workspace's CLI session should
/// launch claude with. CLI-first creation persists the picked selection on the
/// session; chat/automation sessions fall back to the most recent coding-agent
/// run's `executor_config`. `(None, None)` means none was found and the launch
/// uses its own defaults (Opus at max effort).
pub(super) async fn resolve_cli_model_effort(
    pool: &SqlitePool,
    session: Option<&Session>,
) -> (Option<String>, Option<String>) {
    let Some(session) = session else {
        return (None, None);
    };

    // CLI-first: the selection persisted at creation wins.
    if let Ok((model, reasoning)) = Session::get_cli_model_effort(pool, session.id).await
        && (model.is_some() || reasoning.is_some())
    {
        return (model, reasoning);
    }

    // Chat/automation sessions carry the selection in the execution process's
    // action instead (find_by_session_id is ordered created_at ASC, so the most
    // recent coding-agent run is the last match).
    if let Ok(processes) = ExecutionProcess::find_by_session_id(pool, session.id, false).await {
        for process in processes.iter().rev() {
            let Ok(action) = process.executor_action() else {
                continue;
            };
            let config = match action.typ() {
                ExecutorActionType::CodingAgentInitialRequest(request) => {
                    Some(&request.executor_config)
                }
                ExecutorActionType::CodingAgentFollowUpRequest(request) => {
                    Some(&request.executor_config)
                }
                _ => None,
            };
            if let Some(config) = config {
                return (config.model_id.clone(), config.reasoning_id.clone());
            }
        }
    }

    (None, None)
}

/// Resolve how to launch the workspace's selected agent in CLI mode. The agent
/// type comes from the session's `executor` (defaulting to claude); the picked
/// model/effort are folded in as overrides so the interactive launch honors the
/// same selection as headless mode. Agents without their own interactive CLI
/// support fall back to a default claude launch so CLI mode always works.
pub(super) async fn resolve_cli_launch_spec(
    session: Option<&Session>,
    model_id: Option<String>,
    reasoning_id: Option<String>,
    dir: &Path,
    workspace_id: Uuid,
    workspace_name: Option<&str>,
) -> CliLaunchSpec {
    let mut spec = resolve_cli_agent_spec(session, model_id, reasoning_id, dir);

    // Codex reports its state through the same hook endpoint, registered
    // inline with `-c` so neither ~/.codex nor the worktree is touched.
    if spec.program == "codex" {
        spec.session_args = codex_hook_args(&spec.base_args, workspace_id).await;
        let unrestricted = runs_unrestricted(&spec);
        if unrestricted {
            spec.session_args.extend(codex_mcp_args().await);
        }
        spec.session_args
            .extend(codex_lean_mcp_args(workspace_id, unrestricted).await);
    }

    // Everything else keeps the tmux poller's guess.
    if spec.program == "claude" {
        // `--mcp-config` takes several values, so it must be followed by
        // another flag or it would swallow a positional prompt. It is only
        // ever pushed right before `--settings`.
        let mcp_config = write_cli_mcp_config(workspace_id, runs_unrestricted(&spec)).await;
        if let Some(settings) = write_cli_hook_settings(workspace_id).await {
            if let Some((mcp_config, strict)) = mcp_config {
                if strict {
                    spec.base_args.push("--strict-mcp-config".to_string());
                }
                spec.base_args.push("--mcp-config".to_string());
                spec.base_args
                    .push(mcp_config.to_string_lossy().into_owned());
            }
            spec.base_args.push("--settings".to_string());
            spec.base_args.push(settings.to_string_lossy().into_owned());
        }
        if let Some(name) = workspace_name.map(str::trim).filter(|n| !n.is_empty()) {
            spec.base_args.push("--name".to_string());
            spec.base_args.push(name.to_string());
        }
    }

    spec
}

/// The side-effect-free part of [`resolve_cli_launch_spec`]: the selected
/// agent's interactive spec with model/effort folded in, before any hook
/// settings are written. Its `program` is the binary CLI mode launches.
fn resolve_cli_agent_spec(
    session: Option<&Session>,
    model_id: Option<String>,
    reasoning_id: Option<String>,
    dir: &Path,
) -> CliLaunchSpec {
    let executor = session
        .and_then(|s| s.executor.as_deref())
        .and_then(|e| BaseCodingAgent::from_str(e).ok())
        .unwrap_or(BaseCodingAgent::ClaudeCode);

    let profiles = ExecutorConfigs::get_cached();

    let mut config = ExecutorConfig::new(executor);
    config.model_id = model_id;
    config.reasoning_id = reasoning_id;
    let mut agent = profiles.get_coding_agent_or_default(&config.profile_id());
    if config.has_overrides() {
        agent.apply_overrides(&config);
    }

    agent.interactive_cli_spec(dir).unwrap_or_else(|| {
        // The selected agent has no interactive CLI support (yet) — fall back to
        // a default claude launch so the CLI pane is never left without an agent.
        let claude = ExecutorConfig::new(BaseCodingAgent::ClaudeCode);
        profiles
            .get_coding_agent_or_default(&claude.profile_id())
            .interactive_cli_spec(dir)
            .expect("claude always provides an interactive CLI spec")
    })
}

/// The hook command itself: POST stdin to the workspace's activity endpoint.
///
/// Every part of this is about NOT being able to hurt the agent. Claude reads
/// a non-zero exit from a hook as a block, so the command ends in `|| true`;
/// `-m 2` keeps it under the 3s hook timeout even against a wedged socket; and
/// the output goes to /dev/null so nothing is ever injected into the
/// conversation. `date +%s%N` orders reports by when they FIRED rather than
/// when they arrived; a `date` without `%N` just leaves the server to order
/// them by receipt.
fn hook_report_command(port: u16, workspace_id: Uuid) -> String {
    format!(
        "curl -s -m 2 -o /dev/null -X POST -H 'Content-Type: application/json' -H \"x-bc-handover-capability: $BC_HANDOVER_CAPABILITY\" \
         --data-binary @- \
         \"http://127.0.0.1:{port}/api/workspaces/{workspace_id}/cli-activity?seq=$(date +%s%N)\" \
         || true"
    )
}

/// Codex flags that register the activity hooks for this launch, or none.
///
/// Codex runs an inline hook only with `--dangerously-bypass-hook-trust`,
/// which also stops it asking before it runs hooks a repository ships. That
/// adds nothing to a `danger-full-access` session, which can already run any
/// command unasked, so hooks are only registered in that sandbox mode; a
/// restricted session keeps the tmux poller's guess and its trust prompts.
async fn codex_hook_args(base_args: &[String], workspace_id: Uuid) -> Vec<String> {
    if !codex_full_access(base_args) {
        return Vec::new();
    }
    let Ok(port) = utils::port_file::read_port_file("bettercoding").await else {
        tracing::debug!("no port file; codex activity hooks not registered");
        return Vec::new();
    };
    codex_hook_args_for(port, workspace_id)
}

/// The inline `-c hooks.<Event>=[...]` overrides, one per event Codex fires.
fn codex_hook_args_for(port: u16, workspace_id: Uuid) -> Vec<String> {
    // A JSON string literal is a valid TOML basic string.
    let command = serde_json::to_string(&hook_report_command(port, workspace_id))
        .expect("a string always serializes");
    let mut args = vec!["--dangerously-bypass-hook-trust".to_string()];
    for event in CODEX_HOOK_EVENTS.iter().chain(CODEX_ONLY_HOOK_EVENTS) {
        args.push("-c".to_string());
        args.push(format!(
            "hooks.{event}=[{{hooks=[{{type=\"command\",command={command},timeout=3}}]}}]"
        ));
    }
    args
}

fn codex_full_access(base_args: &[String]) -> bool {
    base_args
        .windows(2)
        .any(|pair| pair[0] == "-s" && pair[1] == "danger-full-access")
}

/// Whether the agent already runs any command without asking. Only such an
/// agent gets the BetterCoding MCP: its tools start and message unrestricted
/// agents in other workspaces, which would let a sandboxed agent escape its
/// sandbox.
fn runs_unrestricted(spec: &CliLaunchSpec) -> bool {
    match spec.program.as_str() {
        "claude" => spec
            .base_args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions"),
        "codex" => codex_full_access(&spec.base_args),
        _ => false,
    }
}

/// Name of the MCP server every unrestricted CLI agent is launched with.
const MCP_SERVER_NAME: &str = "bettercoding";

/// How to start this server's own MCP: the running binary's `mcp` subcommand,
/// pointed at this server. `None` when either is unknown.
async fn mcp_server_launch() -> Option<(String, String)> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe.to_string_lossy().into_owned(),
        Err(error) => {
            tracing::debug!(?error, "no current exe; MCP not registered");
            return None;
        }
    };
    let port = utils::port_file::read_port_file("bettercoding")
        .await
        .ok()?;
    Some((exe, format!("http://127.0.0.1:{port}")))
}

/// The user's own MCP servers a workspace agent starts with, next to
/// BetterCoding's. Every other server in `~/.claude.json` and every plugin
/// server stays off: a stdio server is one or more processes per agent, and
/// dozens of agents each starting all of them overload the host.
const LEAN_MCP_SERVERS: &[&str] = &[
    "slack",
    "linear-server",
    "render",
    "Neon",
    "posthog",
    "grafana",
    "patricia",
    "context7",
    "fff",
];

/// Write the per-workspace MCP config for `claude --mcp-config` and return its
/// path with whether to pass `--strict-mcp-config`. It holds the lean set of
/// the user's servers and, for an unrestricted agent only, the BetterCoding
/// tools (create, update, archive and delete workspaces, sessions, repos).
/// The user's config is only read, never edited.
async fn write_cli_mcp_config(workspace_id: Uuid, unrestricted: bool) -> Option<(PathBuf, bool)> {
    let launch = if unrestricted {
        Some(mcp_server_launch().await?)
    } else {
        None
    };
    let dir = utils::assets::asset_dir().join("cli-mcp");
    if let Err(error) = tokio::fs::create_dir_all(&dir).await {
        tracing::debug!(?error, "could not create the CLI MCP config directory");
        return None;
    }
    let names = lean_mcp_names(workspace_mcp_override(workspace_id).await.as_ref());
    let user_config = match dirs::home_dir() {
        Some(home) => tokio::fs::read_to_string(home.join(".claude.json"))
            .await
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok()),
        None => None,
    };
    let lean = cli_mcp_servers(user_config.as_ref(), names.as_deref());
    let strict = lean.is_some();
    let mut servers = lean.unwrap_or_default();
    if let Some((exe, url)) = launch {
        servers.insert(
            MCP_SERVER_NAME.to_string(),
            serde_json::json!({
                "type": "stdio",
                "command": exe,
                "args": ["mcp"],
                "env": { "BC_BACKEND_URL": url },
            }),
        );
    }
    if servers.is_empty() && !strict {
        return None;
    }
    let config = serde_json::json!({ "mcpServers": servers });
    let path = dir.join(format!("{workspace_id}.json"));
    // The copied server entries can carry the user's API keys.
    match write_private(&path, config.to_string().as_bytes()).await {
        Ok(()) => Some((path, strict)),
        Err(error) => {
            tracing::debug!(?error, "could not write the CLI MCP config file");
            None
        }
    }
}

/// The per-workspace override `cli-mcp/<workspace_id>.servers.json`: a JSON
/// array of extra server names (e.g. `["playwright"]`), or `"all"` to keep
/// every server the user configured.
async fn workspace_mcp_override(workspace_id: Uuid) -> Option<serde_json::Value> {
    let path = utils::assets::asset_dir()
        .join("cli-mcp")
        .join(format!("{workspace_id}.servers.json"));
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&text).ok()
}

/// The user's server names an agent keeps: the lean set plus the override's
/// extras, or `None` when the override keeps them all.
fn lean_mcp_names(workspace_override: Option<&serde_json::Value>) -> Option<Vec<String>> {
    if workspace_override.and_then(|v| v.as_str()) == Some("all") {
        return None;
    }
    let extra = workspace_override
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str());
    Some(
        LEAN_MCP_SERVERS
            .iter()
            .copied()
            .chain(extra)
            .map(str::to_string)
            .collect(),
    )
}

/// The user's servers for a strict claude launch, or `None` to keep all of
/// them: every name is kept, or the user's config cannot be read (a strict
/// launch then would silently drop every server).
fn cli_mcp_servers(
    user_config: Option<&serde_json::Value>,
    names: Option<&[String]>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let configured = user_config?.get("mcpServers")?.as_object()?;
    Some(
        names?
            .iter()
            .filter_map(|name| Some((name.clone(), configured.get(name)?.clone())))
            .collect(),
    )
}

/// The `-c mcp_servers.<name>.enabled=false` overrides that turn off every
/// server in `~/.codex/config.toml` outside the lean set. Codex has no strict
/// mode, so each extra server is switched off by name. An unrestricted agent
/// keeps `bettercoding`; a restricted one loses it like any other server.
async fn codex_lean_mcp_args(workspace_id: Uuid, unrestricted: bool) -> Vec<String> {
    let Some(names) = lean_mcp_names(workspace_mcp_override(workspace_id).await.as_ref()) else {
        return Vec::new();
    };
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")));
    let config = match codex_home {
        Some(dir) => tokio::fs::read_to_string(dir.join("config.toml"))
            .await
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok()),
        None => None,
    };
    codex_lean_mcp_args_for(config.as_ref(), &names, unrestricted)
}

fn codex_lean_mcp_args_for(
    config: Option<&toml::Table>,
    names: &[String],
    unrestricted: bool,
) -> Vec<String> {
    let Some(configured) = config
        .and_then(|c| c.get("mcp_servers"))
        .and_then(|v| v.as_table())
    else {
        return Vec::new();
    };
    configured
        .keys()
        .filter(|name| !names.contains(name))
        .filter(|name| !(unrestricted && name.as_str() == MCP_SERVER_NAME))
        // Only a bare TOML key can be named in a `-c` path.
        .filter(|name| {
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .flat_map(|name| {
            [
                "-c".to_string(),
                format!("mcp_servers.{name}.enabled=false"),
            ]
        })
        .collect()
}

async fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        options.mode(0o600);
        // An older launch created the file with the default mode.
        if tokio::fs::metadata(path).await.is_ok() {
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
        }
    }
    let mut file = options.open(path).await?;
    file.write_all(contents).await?;
    file.flush().await
}

/// The inline `-c mcp_servers.<name>.*` overrides that give codex the same MCP
/// server, without touching `~/.codex`.
async fn codex_mcp_args() -> Vec<String> {
    match mcp_server_launch().await {
        Some((exe, url)) => codex_mcp_args_for(&exe, &url),
        None => Vec::new(),
    }
}

fn codex_mcp_args_for(exe: &str, url: &str) -> Vec<String> {
    // A JSON string literal is a valid TOML basic string.
    let quote = |value: &str| serde_json::to_string(value).expect("a string always serializes");
    let key = format!("mcp_servers.{MCP_SERVER_NAME}");
    vec![
        "-c".to_string(),
        format!("{key}.command={}", quote(exe)),
        "-c".to_string(),
        format!("{key}.args=[\"mcp\"]"),
        "-c".to_string(),
        format!("{key}.env={{BC_BACKEND_URL={}}}", quote(url)),
    ]
}

/// Write the per-workspace settings file that registers the activity hooks, and
/// return its path for `claude --settings`.
///
/// `--settings` MERGES with the user's own `~/.claude/settings.json` rather
/// than replacing it, which is the whole reason to use it: bettercoding never
/// edits the user's global config, and never puts a settings file in the
/// worktree where it would show up in `git status`. The file is rewritten on
/// every launch, so the baked-in port is the port of the server doing the
/// launching.
///
/// Returns `None` on any failure. A missing activity signal is a worse sidebar,
/// not a broken launch.
async fn write_cli_hook_settings(workspace_id: Uuid) -> Option<PathBuf> {
    let port = match utils::port_file::read_port_file("bettercoding").await {
        Ok(port) => port,
        Err(error) => {
            tracing::debug!(?error, "no port file; CLI activity hooks not registered");
            return None;
        }
    };

    let entry = serde_json::json!([{
        "hooks": [{
            "type": "command",
            "command": hook_report_command(port, workspace_id),
            "timeout": 3,
        }]
    }]);
    let hooks: serde_json::Map<String, serde_json::Value> = HOOK_EVENTS
        .iter()
        .map(|event| ((*event).to_string(), entry.clone()))
        .collect();
    let settings = serde_json::json!({ "hooks": hooks });

    let dir = utils::assets::asset_dir().join("cli-hooks");
    if let Err(error) = tokio::fs::create_dir_all(&dir).await {
        tracing::debug!(?error, "could not create the CLI hook settings directory");
        return None;
    }
    let path = dir.join(format!("{workspace_id}.json"));
    match tokio::fs::write(&path, settings.to_string()).await {
        Ok(()) => Some(path),
        Err(error) => {
            tracing::debug!(?error, "could not write the CLI hook settings file");
            None
        }
    }
}

/// The workspace's directory, which must exist for any terminal to open in it.
#[allow(clippy::result_large_err)]
fn workspace_dir(workspace: &Workspace) -> Result<PathBuf, ApiError> {
    let container_ref = workspace
        .container_ref
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("Attempt has no workspace directory".to_string()))?;

    let base_dir = PathBuf::from(container_ref);
    if !base_dir.exists() {
        return Err(ApiError::BadRequest(
            "Workspace directory does not exist".to_string(),
        ));
    }
    Ok(base_dir)
}

/// A CLI attach resolved and ready to spawn.
struct CliAttach {
    working_dir: PathBuf,
    command: PtyCommand,
    prompt_delivery: Option<PromptDelivery>,
    registration: Option<CliLaunchRegistration>,
}

/// Resolve a CLI attach: the session, directory and agent launch, whether this
/// attach launches a fresh pane, and the parked prompt it carries. Shared by
/// the terminal WebSocket and the headless launch.
async fn spawn_cli_terminal(
    deployment: &DeploymentImpl,
    workspace: &Workspace,
    base_dir: &Path,
    requested_session_id: Option<Uuid>,
    connect_hidden: bool,
    dimensions: (u16, u16),
) -> Result<SpawnedTerminal, ApiError> {
    let _guard = super::cli_agent::cli_lifecycle_lock(workspace.id)
        .lock_owned()
        .await;
    let pool = &deployment.db().pool;
    let _writer = local_deployment::pty::cli_writer_admission(pool, workspace.id)
        .await
        .map_err(|e| ApiError::Conflict(e.to_string()))?;
    let reservation = if cli_tmux_available()
        && cli_tmux_session_exists_checked(workspace.id).await?
    {
        None
    } else {
        let mut acquired = None;
        for attempt in 0..10 {
            acquired =
                WorkspaceSpawnReservation::acquire(pool, workspace.id, SpawnReservationHolder::Cli)
                    .await?;
            if acquired.is_some() {
                break;
            }
            if attempt < 9 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        Some(acquired.ok_or_else(|| {
            ApiError::Conflict(
                "Another writer is starting for this workspace; retry the terminal attach".into(),
            )
        })?)
    };
    let result = async {
        let attach = prepare_cli_attach(
            deployment,
            workspace,
            base_dir,
            requested_session_id,
            connect_hidden,
            reservation.is_none(),
        )
        .await?;
        let spawned = spawn_terminal(
            deployment,
            attach.working_dir,
            dimensions,
            attach.command,
            attach.prompt_delivery,
            attach.registration,
        )
        .await
        .map_err(ApiError::Conflict)?;
        if cli_tmux_available()
            && let Err(error) = deployment
                .pty()
                .wait_for_cli_attach(spawned.session_id)
                .await
        {
            let _ = deployment.pty().close_session(spawned.session_id).await;
            return Err(ApiError::Pty(error));
        }
        if cli_tmux_available()
            && let Err(error) = deployment
                .pty()
                .bind_cli_attachment(spawned.session_id, workspace.id)
                .await
        {
            let _ = deployment.pty().close_session(spawned.session_id).await;
            return Err(ApiError::Pty(error));
        }
        Ok(spawned)
    }
    .await;
    if let Some(reservation) = reservation
        && let Err(error) =
            WorkspaceSpawnReservation::release(pool, workspace.id, &reservation.fence).await
    {
        if let Ok(spawned) = &result {
            let _ = deployment.pty().close_session(spawned.session_id).await;
        }
        return Err(error.into());
    }
    result
}

async fn prepare_cli_attach(
    deployment: &DeploymentImpl,
    workspace: &Workspace,
    base_dir: &Path,
    requested_session_id: Option<Uuid>,
    connect_hidden: bool,
    attach_only: bool,
) -> Result<CliAttach, ApiError> {
    let mut prompt_delivery: Option<PromptDelivery> = None;
    let mut cli_launch_registration: Option<CliLaunchRegistration> = None;
    let pool = &deployment.db().pool;

    let session = Session::find_for_cli(pool, workspace.id, requested_session_id).await?;

    // Run claude exactly where the coding agent runs: the workspace
    // root plus the session's relative agent_working_dir (mirrors
    // CodingAgentInitialRequest::effective_dir). claude keys
    // conversation storage by cwd, so --resume/--continue only find
    // the executor's transcript from that directory.
    let mut dir = base_dir.to_path_buf();
    if let Some(rel) = session
        .as_ref()
        .and_then(|s| s.agent_working_dir.as_deref())
        .filter(|d| !d.is_empty())
    {
        let candidate = base_dir.join(rel);
        if candidate.exists() {
            dir = candidate;
        }
    }

    // Resolve claude's session id for the selected uix chat so CLI
    // mode resumes the exact conversation (handover). With no prior
    // turn the bootstrap starts a fresh TUI. While the headless
    // executor is actively RUNNING this session, never hand its id to
    // a second claude — resuming a session mid-write forks it and the
    // user ends up with chat and CLI doing the same work twice.
    let (executor_active, known_claude_session_id) = match &session {
        Some(s) => {
            let executor_active = match ExecutionProcess::has_running_coding_agent_for_session(
                pool, s.id,
            )
            .await
            {
                Ok(active) => active,
                Err(error) => {
                    tracing::warn!(?error, session_id = %s.id, "terminal writer guard failed closed");
                    true
                }
            };
            let sid = CliPaneBinding::resume_session_id(pool, s.id).await?;
            (executor_active, sid)
        }
        None => (false, None),
    };
    let resume_session_id = (!executor_active)
        .then(|| known_claude_session_id.clone())
        .flatten();
    let tmux_available = cli_tmux_available();
    let cli_session_exists = if tmux_available {
        cli_tmux_session_exists_checked(workspace.id).await?
    } else {
        false
    };
    if tmux_available && !attach_only && !cli_session_exists {
        let launch_session = session.as_ref().ok_or_else(|| {
            ApiError::BadRequest(
                "CLI mode requires a BetterCoding session for this workspace".to_string(),
            )
        })?;
        if executor_active {
            remove_cli_resume_ready_file(workspace.id);
        }
        cli_launch_registration = Some(CliLaunchRegistration {
            workspace_id: workspace.id,
            session_id: launch_session.id,
            claude_session_id: known_claude_session_id.clone(),
            bound_via: if known_claude_session_id.is_some() {
                CliPaneBoundVia::CliResume
            } else {
                CliPaneBoundVia::CliFresh
            },
            cwd: dir.to_string_lossy().into_owned(),
        });
    }

    // A parked prompt (CLI-first creation, or a re-parked loop wake-up)
    // is only ever PEEKED here (read, don't clear); the clear happens
    // after delivery is CONFIRMED (see handle_terminal_ws), so no
    // failure between WS upgrade and agent hand-off can destroy the
    // user's only copy. Racing attaches are serialized by the
    // CliPromptDelivery claim — the loser attaches without the prompt.
    //
    // Gate on tmux availability: with tmux down, CLI mode degrades to
    // an ephemeral shell that can't run claude, so the bootstrap would
    // never deliver the prompt — peeking+clearing it would lose it.
    // Since availability is process-cached, `true` here means
    // `create_session` also takes the tmux branch, so a successful
    // spawn really did carry the prompt into a tmux session.
    // Workspace-scoped peek: creation parks on the CLI-first session,
    // loop wake-ups re-park on the LATEST session, and this attach may
    // have resolved a third (frontend-selected) session — a
    // session-scoped peek would strand a prompt parked on a sibling
    // row.
    let carried: Option<(String, CliPromptDelivery, Uuid)> = if cli_tmux_available() {
        match CliPromptDelivery::try_claim(workspace.id) {
            Some(claim) => {
                match Session::peek_pending_cli_prompt_for_workspace(pool, workspace.id).await {
                    Ok(Some((owning_session_id, prompt))) => {
                        Some((prompt, claim, owning_session_id))
                    }
                    // Nothing parked: release the claim (drop).
                    Ok(None) => None,
                    Err(e) => {
                        // The prompt is not lost — it stays parked and
                        // the next attach re-peeks — but a transient DB
                        // error here delays delivery, so make it
                        // observable.
                        tracing::warn!(
                            "Failed to read pending CLI prompt for workspace {}: {}",
                            workspace.id,
                            e
                        );
                        None
                    }
                }
            }
            None => {
                tracing::debug!(
                    "CLI prompt delivery for workspace {} already in flight; \
                     attaching without the prompt",
                    workspace.id
                );
                None
            }
        }
    } else {
        None
    };
    // Honor the workspace's selected agent + model/effort at launch
    // (defaults to claude at Opus/max when nothing was selected).
    let (model_id, reasoning_id) = resolve_cli_model_effort(pool, session.as_ref()).await;
    let spec = resolve_cli_launch_spec(
        session.as_ref(),
        model_id,
        reasoning_id,
        &dir,
        workspace.id,
        workspace.name.as_deref(),
    )
    .await;

    // How the parked prompt travels:
    // - Genuine first attach (no tmux session yet, nothing to resume):
    //   small prompts ride the bootstrap's temp-file transport (baked
    //   into the launch); prompts too large for one argv entry are
    //   pasted after the agent owns the pane.
    // - Otherwise (the session already exists — e.g. an earlier
    //   delivery went unconfirmed, or a loop wake-up was re-parked —
    //   or an existing conversation is being resumed, which always
    //   wins the launch itself): deliver by paste into the running
    //   agent, as a follow-up. Without this branch a parked prompt
    //   behind a live session or a resume would be stranded forever.
    // Either way the parked prompt is cleared only after delivery is
    // confirmed.
    let mut baked_prompt = None;
    let mut deferred_prompt_pending = CliPaneBinding::find_active_for_workspace(pool, workspace.id)
        .await?
        .is_some_and(|binding| {
            binding.bound_via == CliPaneBoundVia::CliFresh && binding.claude_session_id.is_none()
        });
    if let Some((peeked, claim, clear_session_id)) = carried {
        // Match the bootstrap's resume predicate EXACTLY (it filters the
        // id through is_uuid before resuming): a non-UUID resume id is
        // NOT resumed by the launch, so it must count as a fresh launch
        // here too. Otherwise the prompt would route as a follow-up
        // paste into a continue_launch's doomed `--continue` leg (the
        // same loss hazard the racing-attach path avoids).
        let resume_will_apply = resume_session_id
            .as_deref()
            .is_some_and(|id| Uuid::parse_str(id).is_ok());
        let fresh_launch = !resume_will_apply && !cli_session_exists;
        let routed = if fresh_launch {
            route_initial_prompt(Some(peeked.clone()), &spec.prompt_arg)
        } else {
            route_followup_prompt(&peeked)
        };
        match routed {
            CliPromptRouting::None => {
                // Blank-after-trim: nothing can ever be delivered, so
                // clear the parked blank (CAS keeps a newer prompt
                // safe) instead of re-claiming and re-probing it on
                // every future attach. The claim drops here.
                match Session::clear_pending_cli_prompt(pool, clear_session_id, &peeked).await {
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        "Failed to clear blank parked CLI prompt for session {}: {}",
                        clear_session_id,
                        e
                    ),
                }
            }
            CliPromptRouting::Baked(prompt) => {
                baked_prompt = Some(prompt);
                prompt_delivery = Some(PromptDelivery {
                    workspace_id: workspace.id,
                    clear_session_id,
                    peeked,
                    deferred: None,
                    program: spec.program.clone(),
                    claim,
                });
            }
            CliPromptRouting::Deferred(prompt) => {
                // Request the bare-TUI bootstrap whenever a resume won't
                // apply — not just when the session is currently absent.
                // A live session can exit between this check and
                // `new-session -A`; if it does, the freshly created pane
                // must NOT run `continue_launch`'s doomed `--continue`
                // leg (the deferred paste could land on it and be lost).
                // When the session survives, the bootstrap is ignored by
                // `-A`, so requesting it is harmless.
                deferred_prompt_pending = !resume_will_apply;
                prompt_delivery = Some(PromptDelivery {
                    workspace_id: workspace.id,
                    clear_session_id,
                    peeked,
                    deferred: Some(prompt),
                    program: spec.program.clone(),
                    claim,
                });
            }
        }
    }

    Ok(CliAttach {
        working_dir: dir,
        command: PtyCommand::TmuxCli {
            workspace_id: workspace.id,
            attach_only,
            resume_session_id,
            busy_wait_for_resume: executor_active,
            initial_prompt: baked_prompt,
            deferred_prompt_pending,
            connect_hidden,
            spec,
        },
        prompt_delivery,
        registration: cli_launch_registration,
    })
}

async fn terminal_ws(
    ws: SignedWsUpgrade,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<TerminalQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let attempt = Workspace::find_by_id(&deployment.db().pool, query.workspace_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Attempt not found".to_string()))?;

    let base_dir = workspace_dir(&attempt)?;

    let dimensions = (
        query.cols.unwrap_or(DEFAULT_COLS),
        query.rows.unwrap_or(DEFAULT_ROWS),
    );
    let shell_dir = match query.mode {
        TerminalMode::Cli => {
            // Regression tripwire for the stray-newline bug: a CLI attach must
            // always carry the pane's real fitted grid. Absent cols/rows mean
            // the frontend connected without measuring, which makes claude
            // reflow and stack blank lines on the follow-up resize. After the
            // frontend fix (never connect at an unmeasured size, URL always
            // carries the fitted grid) this must never fire. An EXPLICIT
            // 80x24 is legitimate (a pane can really measure that); a
            // literal-default regression stays observable via the per-attach
            // cols x rows log in pty.rs.
            if query.cols.is_none() || query.rows.is_none() {
                tracing::warn!(
                    "CLI terminal attaching without cols/rows for workspace {} — \
                     frontend connected unmeasured (stray-newline regression)",
                    query.workspace_id
                );
            }

            None
        }
        // Side terminals open in the repo for single-repo workspaces — the
        // most useful default for running project commands by hand.
        TerminalMode::Shell => {
            let mut dir = base_dir.clone();
            match WorkspaceRepo::find_repos_for_workspace(&deployment.db().pool, query.workspace_id)
                .await
            {
                Ok(repos) if repos.len() == 1 => {
                    let repo_dir = base_dir.join(&repos[0].name);
                    if repo_dir.exists() {
                        dir = repo_dir;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "Failed to resolve repos for workspace {}: {}",
                        attempt.id,
                        e
                    );
                }
            }
            Some(dir)
        }
    };

    Ok(ws.on_upgrade(move |mut socket| async move {
        let spawned = match shell_dir {
            Some(dir) => {
                spawn_terminal(&deployment, dir, dimensions, PtyCommand::Shell, None, None)
                    .await
                    .map_err(ApiError::Conflict)
            }
            None => {
                spawn_cli_terminal(
                    &deployment,
                    &attempt,
                    &base_dir,
                    query.session_id,
                    query.hidden,
                    dimensions,
                )
                .await
            }
        };
        match spawned {
            Ok(spawned) => handle_terminal_ws(socket, deployment, spawned).await,
            Err(error) => {
                let _ = send_error(&mut socket, &error.to_string()).await;
            }
        }
    }))
}

/// Everything a prompt-carrying CLI attach needs to confirm delivery and clear
/// the parked prompt. Built only when this attach actually carries the prompt
/// (it holds the claim), so "claim without prompt" or "deferred text without a
/// session to clear" are unrepresentable.
struct PromptDelivery {
    workspace_id: Uuid,
    /// Session whose parked prompt to clear once delivery is confirmed.
    clear_session_id: Uuid,
    /// The exact parked value that was peeked — the clear is a compare-and-swap
    /// against it, so a NEWER prompt parked mid-confirmation (e.g. a loop
    /// wake-up re-parked while this delivery was in flight) is never destroyed
    /// by this delivery's clear.
    peeked: String,
    /// Prompt to paste once the agent owns the pane (`None` = baked into the
    /// launch's temp-file transport instead).
    deferred: Option<String>,
    /// The agent binary this launch runs — delivery is only ever confirmed
    /// against THIS process owning the pane, so a fallback shell (or anything
    /// the user runs inside it) can never satisfy the gate.
    program: String,
    /// Exclusive in-process claim; held until the parked copy is cleared or
    /// deliberately left parked, releasing on drop.
    claim: CliPromptDelivery,
}

struct CliLaunchRegistration {
    workspace_id: Uuid,
    session_id: Uuid,
    claude_session_id: Option<String>,
    bound_via: CliPaneBoundVia,
    cwd: String,
}

/// A spawned terminal: its PTY session and output stream.
struct SpawnedTerminal {
    session_id: Uuid,
    output_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    tripwire_session: String,
}

/// Spawn the PTY for an attach, register a fresh CLI launch, and start
/// delivering the parked prompt it carries. `Err` is the message for the
/// client; nothing is left behind on that path.
async fn spawn_terminal(
    deployment: &DeploymentImpl,
    working_dir: PathBuf,
    dimensions: (u16, u16),
    command: PtyCommand,
    prompt_delivery: Option<PromptDelivery>,
    cli_launch_registration: Option<CliLaunchRegistration>,
) -> Result<SpawnedTerminal, String> {
    let (cols, rows) = dimensions;
    // FIX 4 tripwire label: the pty session name, captured before `command` is
    // moved into `create_session`. For CLI mode this is the `bc_<uuid>` name,
    // so the bytes line up with tmux logs.
    let tripwire_session = match &command {
        PtyCommand::TmuxCli { workspace_id, .. } => cli_tmux_session_name(*workspace_id),
        PtyCommand::Shell => "shell".to_string(),
    };

    let (session_id, output_rx) = match deployment
        .pty()
        .create_session(working_dir, cols, rows, command)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("Failed to create PTY session: {}", e);
            // A prompt file staged before the spawn failure must not outlive
            // it (the DB copy stays parked for the next attach; the claim is
            // released on return).
            if let Some(delivery) = &prompt_delivery {
                remove_cli_prompt_file(delivery.workspace_id);
            }
            return Err(e.to_string());
        }
    };

    if let Some(registration) = cli_launch_registration {
        let registered = match wait_for_cli_session(registration.workspace_id).await {
            Ok(true) => register_cli_launch(deployment, &registration).await,
            Ok(false) => Err(ApiError::Conflict(
                "CLI tmux session did not become ready".to_string(),
            )),
            Err(error) => Err(ApiError::Pty(error)),
        };
        if let Err(error) = registered {
            kill_cli_tmux_session_admitted(
                &deployment.db().pool,
                registration.workspace_id,
                "spawn_terminal:registration_failed",
            )
            .await
            .map_err(|e| e.to_string())?;
            if let Some(delivery) = &prompt_delivery {
                remove_cli_prompt_file(delivery.workspace_id);
            }
            let _ = deployment.pty().close_session(session_id).await;
            return Err(error.to_string());
        }
    }

    if let Some(delivery) = prompt_delivery {
        let workspace_id = delivery.workspace_id;
        if matches!(wait_for_cli_session(workspace_id).await, Ok(true)) {
            // Confirm delivery and clear the parked prompt in the background so
            // the pane's output starts streaming immediately — the confirmation
            // polls below would otherwise hold the whole terminal blank.
            let pool = deployment.db().pool.clone();
            tokio::spawn(async move {
                // Hold the exclusive delivery claim for the whole
                // confirm-then-clear window.
                let _claim = delivery.claim;
                let delivered = match &delivery.deferred {
                    Some(text) => {
                        deliver_deferred_prompt(&pool, workspace_id, text, &delivery.program).await
                    }
                    None => confirm_baked_prompt_consumed(workspace_id, &delivery.program).await,
                };
                if delivered {
                    match Session::clear_pending_cli_prompt(
                        &pool,
                        delivery.clear_session_id,
                        &delivery.peeked,
                    )
                    .await
                    {
                        // Superseded: a newer prompt was parked while this one
                        // was being confirmed; it stays parked for its own
                        // delivery on the next attach.
                        Ok(false) => tracing::info!(
                            "Parked CLI prompt for session {} changed during delivery; \
                             left for the next attach",
                            delivery.clear_session_id
                        ),
                        Ok(true) => {}
                        Err(e) => tracing::warn!(
                            "Failed to clear delivered CLI prompt for session {}: {}",
                            delivery.clear_session_id,
                            e
                        ),
                    }
                } else {
                    // Leave the prompt parked; the next attach retries (paste
                    // into the live pane, or a fresh launch after the session
                    // dies) — delivery is only ever confirmed, never assumed.
                    tracing::warn!(
                        "CLI prompt delivery for workspace {} unconfirmed; left parked",
                        workspace_id
                    );
                }
            });
        } else {
            // The session never appeared — remove the transient prompt file
            // (the DB copy stays parked) and surface the failure. Deliberately
            // NOT killing a session that squeaks in after the poll window: a
            // slow-but-successful launch may already be running the agent (or
            // hosting another attach's client), and murdering it loses real
            // work — while a parked prompt behind a live session is now
            // recoverable anyway (the next attach delivers it by paste). The
            // frontend renders the error in red and halts its reconnect loop.
            remove_cli_prompt_file(workspace_id);
            tracing::error!(
                "CLI tmux session for workspace {} never came up; prompt left parked",
                workspace_id
            );
            // Tear down the PtyService entry for the (already-dead) tmux client
            // before bailing; otherwise this early return leaks the session map
            // entry — one per failed attach — that the normal exit path below
            // would have reaped via close_session.
            let _ = deployment.pty().close_session(session_id).await;
            return Err(CLI_PROMPT_PARKED_NOTICE.to_string());
        }
    }

    Ok(SpawnedTerminal {
        session_id,
        output_rx,
        tripwire_session,
    })
}

async fn handle_terminal_ws(
    socket: MaybeSignedWebSocket,
    deployment: DeploymentImpl,
    spawned: SpawnedTerminal,
) {
    let SpawnedTerminal {
        session_id,
        output_rx,
        tripwire_session,
    } = spawned;

    // FIX 4 input tripwire — bounds and rationale live on `AttachInputTripwire`.
    let mut tripwire = AttachInputTripwire::new(tripwire_session, session_id);

    let pty_service = deployment.pty().clone();
    let session_id_for_input = session_id;

    let signed_transport = socket.is_signed();
    run_terminal_transport(
        socket,
        output_rx,
        signed_transport,
        |cmd| {
            if let TerminalCommand::Input { data } = &cmd
                && let Ok(bytes) = BASE64.decode(data)
            {
                tripwire.observe(&bytes);
            }
            let pty_service = &pty_service;
            async move {
                match cmd {
                    TerminalCommand::Input { data } => {
                        if let Ok(bytes) = BASE64.decode(&data) {
                            pty_service
                                .write(session_id_for_input, &bytes)
                                .await
                                .map_err(|e| e.to_string())?;
                        } else {
                            return Err("Invalid terminal input encoding".into());
                        }
                    }
                    TerminalCommand::Resize { cols, rows } => {
                        let _ = pty_service.resize(session_id_for_input, cols, rows).await;
                    }
                    TerminalCommand::Presence { visible } => {
                        pty_service.set_cli_presence(session_id_for_input, visible);
                    }
                }
                Ok(())
            }
        },
        || async {
            let _ = deployment.pty().close_session(session_id).await;
        },
    )
    .await;
}

// Capable signed viewers answer in JavaScript; suspension beyond the response window
// releases this attach only. The native tmux session remains available on reconnect.
const TERMINAL_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
const TERMINAL_HEARTBEAT_RESPONSE: std::time::Duration = std::time::Duration::from_secs(15);
const TERMINAL_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn send_terminal_message<S>(socket: &mut S, message: Message) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = anyhow::Error> + Unpin,
{
    use futures_util::SinkExt;
    // A signed send can advance its sequence before flushing. Every error,
    // including cancellation at this deadline, permanently ends this transport.
    tokio::time::timeout(TERMINAL_SEND_TIMEOUT, socket.send(message)).await??;
    Ok(())
}

// Keep transport independent of PTY creation so regressions exercise the actual loop
// without starting tmux, a native agent, or a database.
async fn run_terminal_transport<S, Command, CommandFuture, Close, CloseFuture>(
    mut socket: S,
    mut output_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    signed_transport: bool,
    mut command: Command,
    close: Close,
) where
    S: futures_util::Stream<Item = anyhow::Result<Message>>
        + futures_util::Sink<Message, Error = anyhow::Error>
        + Unpin,
    Command: FnMut(TerminalCommand) -> CommandFuture,
    CommandFuture: std::future::Future<Output = Result<(), String>>,
    Close: FnOnce() -> CloseFuture,
    CloseFuture: std::future::Future<Output = ()>,
{
    use futures_util::StreamExt;
    use tokio::time::{Instant, sleep_until};

    let mut next_probe = Instant::now() + TERMINAL_HEARTBEAT_INTERVAL;
    let mut outstanding: Option<(Vec<u8>, Instant)> = None;
    // Legacy signed viewers discard probes. Require a verified matching Pong
    // before using missing responses as evidence of a dead signed connection.
    let mut receive_expiry_enabled = !signed_transport;

    loop {
        let deadline = outstanding
            .as_ref()
            .map_or(next_probe, |(_, deadline)| *deadline);
        tokio::select! {
            // Always poll liveness and inbound control frames before a ready PTY stream.
            biased;
            _ = sleep_until(deadline) => {
                if outstanding.is_some() && receive_expiry_enabled {
                    break;
                }
                // Rotate unanswered legacy probes without enabling receive expiry.
                let challenge = Uuid::new_v4().as_bytes().to_vec();
                if send_terminal_message(&mut socket, Message::Ping(challenge.clone().into())).await.is_err() {
                    break;
                }
                outstanding = Some((challenge, Instant::now() + TERMINAL_HEARTBEAT_RESPONSE));
            }
            inbound = socket.next() => {
                match inbound {
                    Some(Ok(Message::Pong(payload))) => {
                        if outstanding.as_ref().is_some_and(|(challenge, _)| challenge.as_slice() == payload.as_ref()) {
                            receive_expiry_enabled = true;
                            outstanding = None;
                            next_probe = Instant::now() + TERMINAL_HEARTBEAT_INTERVAL;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if send_terminal_message(&mut socket, Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<TerminalCommand>(text.as_str()) {
                            // Existing PTY writes/resize are synchronous and can block here.
                            // Their cancellation/worker design requires separate review;
                            // transport deadlines cannot preempt a blocked command.
                            if let Err(message) = command(cmd).await {
                                let error = TerminalMessage::InputRejected { message };
                                let Ok(json) = serde_json::to_string(&error) else { break; };
                                if send_terminal_message(&mut socket, Message::Text(json.into())).await.is_err() { break; }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    None => break,
                    Some(Err(error)) => {
                        tracing::warn!("terminal WS receive error: {}", error);
                        break;
                    }
                }
            }
            maybe_output = output_rx.recv() => {
                let Some(data) = maybe_output else {
                    break;
                };

                let msg = TerminalMessage::Output {
                    data: BASE64.encode(&data),
                };
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(_) => continue,
                };

                if send_terminal_message(&mut socket, Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
        }
    }

    close().await;
}

#[cfg(test)]
#[path = "terminal_transport_tests.rs"]
mod terminal_transport_tests;

/// Poll for the workspace's CLI tmux session to appear after a spawn. The
/// session existing proves tmux accepted `new-session` and the bootstrap (which
/// owns the prompt file) is running. Bounded backoff (~10 × 250ms): the session
/// shows up near-instantly when tmux accepts the command, so the full window is
/// only spent when the launch is failing — and a window that's too short would
/// false-negative on a heavily loaded machine, stranding a prompt that WAS
/// delivered (the replay hazard the deferred clear exists to prevent).
async fn register_cli_launch(
    deployment: &DeploymentImpl,
    registration: &CliLaunchRegistration,
) -> Result<(), ApiError> {
    forget_ended_session(&deployment.db().pool, registration.workspace_id).await;
    let binding = CliPaneBinding::record_launch(
        &deployment.db().pool,
        registration.workspace_id,
        registration.session_id,
        registration.claude_session_id.as_deref(),
        registration.bound_via,
    )
    .await?;
    let Some(sid) = registration.claude_session_id.as_deref() else {
        return Ok(());
    };
    // The ingest service, when running, bumps the feed revisions of both the
    // new and the previous owner so connected chat feeds rebuild now.
    let assigned = match deployment.claude_transcript_ingest() {
        Some(ingest) => {
            ingest
                .assign_cli_launch(
                    sid,
                    registration.session_id,
                    registration.workspace_id,
                    &registration.cwd,
                )
                .await
        }
        None => ClaudeSessionLink::assign_cli(
            &deployment.db().pool,
            sid,
            registration.session_id,
            registration.workspace_id,
            &registration.cwd,
            ClaudeSessionBoundVia::CliResume,
        )
        .await
        .map(|_| ()),
    };
    if let Err(error) = assigned {
        let _ = CliPaneBinding::release(&deployment.db().pool, binding.id).await;
        return Err(error.into());
    }
    Ok(())
}

/// A freshly launched agent replaces the session whose `ended` report the
/// sidebar would otherwise keep showing. Best effort: a failure costs a stale
/// tag, never the launch.
pub(super) async fn forget_ended_session(pool: &SqlitePool, workspace_id: Uuid) {
    if let Err(error) = WorkspaceCliActivity::clear_ended_phase(pool, workspace_id).await {
        tracing::warn!(?error, %workspace_id, "could not clear the ended CLI phase");
    }
}

async fn wait_for_cli_session(workspace_id: Uuid) -> Result<bool, local_deployment::pty::PtyError> {
    for attempt in 0..10 {
        if cli_tmux_session_exists_checked(workspace_id).await? {
            return Ok(true);
        }
        if attempt < 9 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    Ok(false)
}

/// Confirm a baked prompt was actually handed to the agent: the staged file is
/// gone (the bootstrap `rm`s it at hand-off — see [`cli_prompt_file_exists`])
/// AND the expected agent process owns the pane. The second condition catches
/// the consumed-but-never-executed window (`command -v` passed but the agent's
/// exec failed: bad shebang, loader error, permissions) where file-gone alone
/// would clear a prompt no agent ever received. An unconfirmed launch leaves
/// the parked DB prompt for the next attach's paste/fresh-launch retry — and
/// drops the never-consumed file so a later launch can't half-consume it.
/// The tmux target is pinned before polling so a double-home change cannot
/// satisfy the process check from a different session than the recipient.
///
/// Note: a prompt stranded by a losing racing first-attach that won
/// `new-session -A` with a promptless bootstrap is deliberately NOT recovered
/// by pasting here — the loser's `--continue || fresh` leg and the eventual
/// fresh agent share a process name, so a paste could land on the dying
/// `--continue` leg yet be "confirmed" by the fresh one, silently losing the
/// prompt. Leaving it parked means the next attach delivers it as a follow-up
/// paste into the live agent: slower in that rare race, but never lost.
async fn confirm_baked_prompt_consumed(workspace_id: Uuid, program: &str) -> bool {
    let Some(target) = locate_cli_tmux_target(workspace_id).await else {
        remove_cli_prompt_file(workspace_id);
        return false;
    };
    for _ in 0..30 {
        if !cli_prompt_file_exists(workspace_id)
            && cli_pane_agent_running_at(&target, program).await == Some(true)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    remove_cli_prompt_file(workspace_id);
    false
}

/// Deliver a deferred prompt by paste once THE AGENT owns the pane. Pasting
/// earlier would hand the prompt to the bootstrap/fallback shell instead —
/// which executes it as shell input (binary missing) or truncates it at the
/// tty's canonical-mode line limit (TUI not yet in raw mode). Readiness = the
/// expected agent process in the pane on two consecutive polls, re-checked
/// right before the paste (the agent can die inside the grace window — e.g. a
/// failed resume attempt — and the pane fall back to a shell) and re-checked
/// after it (an agent that exited immediately after the paste discarded the
/// text with its tty; delivery must not be confirmed). Bounded at ~15s; an
/// unready pane leaves the prompt parked for the next attach's retry. The tmux
/// target is located once before polling and reused through the post-paste ack,
/// so a mid-delivery home flip cannot redirect any step to the other session.
async fn deliver_deferred_prompt(
    pool: &SqlitePool,
    workspace_id: Uuid,
    text: &str,
    program: &str,
) -> bool {
    let Some(target) = locate_cli_tmux_target(workspace_id).await else {
        return false;
    };
    let mut stable = 0u32;
    for _ in 0..60 {
        match cli_pane_agent_running_at(&target, program).await {
            Some(true) => {
                stable += 1;
                if stable >= 2 {
                    // Grace for the TUI to enter raw mode, then re-verify the
                    // agent still owns the pane immediately before pasting.
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                    if cli_pane_agent_running_at(&target, program).await != Some(true) {
                        stable = 0;
                        continue;
                    }
                    // Re-probed under the send lock: a send queued behind
                    // another must not paste into a shell the agent left.
                    if !send_cli_keys_to_live_agent(pool, &target, &[program], text)
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(CliSendResult::delivered)
                    {
                        return false;
                    }
                    // Post-paste ack: the agent must have survived receiving
                    // it. A process that died right after the paste (doomed
                    // resume leg, instant crash) never processed the text.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    return cli_pane_agent_running_at(&target, program).await == Some(true);
                }
            }
            Some(false) => stable = 0,
            None => {
                // Pane unreadable: if the session is really gone, give up and
                // leave the prompt parked; a transient probe failure (ps/pgrep
                // hiccup) just resets the stability counter.
                if !cli_tmux_target_exists(&target).await {
                    return false;
                }
                stable = 0;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    false
}

/// FIX 4 input tripwire: for a strictly bounded window right after each
/// attach, hex-dump every client→pty input chunk. This is the production
/// canary for the stray-newline / EOT injection bug (portable-pty's
/// writer-Drop wrote `\n` + Ctrl-D into the tmux client's tty on teardown,
/// killing the pane) and for any FUTURE client-side injector: it makes the
/// exact bytes the browser sent — with an ms-since-attach stamp and the
/// per-attach id — visible in server logs, so a client-origin injection can
/// be told from a server-side one. Bounded to the first `Self::WINDOW` OR
/// `Self::MAX_BYTES` (whichever first); once spent, `observe` is a single
/// integer compare with no allocation, so it is permanently safe to leave
/// enabled.
struct AttachInputTripwire {
    /// The pty session name (`bc_<uuid>` in CLI mode) so the logged bytes line
    /// up with tmux server logs.
    session: String,
    /// The per-attach PTY session id.
    attach_id: Uuid,
    started: std::time::Instant,
    bytes_logged: usize,
}

impl AttachInputTripwire {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
    const MAX_BYTES: usize = 128;

    fn new(session: String, attach_id: Uuid) -> Self {
        Self {
            session,
            attach_id,
            started: std::time::Instant::now(),
            bytes_logged: 0,
        }
    }

    /// Log (redacted — see `hex_dump`) input bytes while the attach window is
    /// open. Expiry latches: the monotonic clock never goes backwards, so
    /// marking the byte budget spent on the first out-of-window observation
    /// is behavior-identical and skips even the clock read afterwards.
    fn observe(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.bytes_logged >= Self::MAX_BYTES {
            return;
        }
        let elapsed = self.started.elapsed();
        if elapsed >= Self::WINDOW {
            self.bytes_logged = Self::MAX_BYTES;
            return;
        }
        let take = (Self::MAX_BYTES - self.bytes_logged).min(bytes.len());
        tracing::info!(
            session = %self.session,
            attach = %self.attach_id,
            ms_since_attach = elapsed.as_millis() as u64,
            bytes = %hex_dump(&bytes[..take]),
            "terminal input (attach-window tripwire)"
        );
        self.bytes_logged += take;
    }
}

/// Space-separated redacted hex of a byte slice for the attach-window input
/// tripwire: control bytes (< 0x20, or 0x7f) are shown verbatim (e.g. `0a 04`,
/// `1b`) because they are the injection/escape-sequence signature; every other
/// byte is masked as `..` so real keystrokes (passwords, pasted secrets) never
/// reach the logs — only their count and timing do.
/// The caller bounds the slice length, so this
/// never allocates more than a fixed maximum.
fn hex_dump(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        if *byte < 0x20 || *byte == 0x7f {
            let _ = write!(out, "{byte:02x} ");
        } else {
            out.push_str(".. ");
        }
    }
    out.pop(); // trailing space
    out
}

async fn send_error(socket: &mut MaybeSignedWebSocket, message: &str) -> anyhow::Result<()> {
    let msg = TerminalMessage::Error {
        message: message.to_string(),
    };
    let json = serde_json::to_string(&msg).unwrap_or_default();
    send_terminal_message(socket, Message::Text(json.into())).await?;
    tokio::time::timeout(TERMINAL_SEND_TIMEOUT, socket.close()).await??;
    Ok(())
}

/// Largest composer text `POST /workspaces/{id}/cli/send` accepts, in bytes.
const MAX_CLI_SEND_BYTES: usize = 256 * 1024;

/// Check composer text before it reaches the pane. Control characters other
/// than newline and tab are refused: an ESC could end the bracketed paste
/// early (`ESC [201~`) and turn the rest of the text into TUI key input.
/// Surrounding newlines are dropped; everything else is sent verbatim.
fn validate_cli_send_text(text: &str) -> Result<&str, String> {
    if text.len() > MAX_CLI_SEND_BYTES {
        return Err(format!(
            "Text is {} bytes; the limit is {MAX_CLI_SEND_BYTES} bytes",
            text.len()
        ));
    }
    if text.trim().is_empty() {
        return Err("Text is empty".to_string());
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("Text contains control characters".to_string());
    }
    Ok(text.trim_matches('\n'))
}

/// Type composer text into the workspace's live CLI agent pane and submit it.
/// 409 when no Claude/Codex agent owns the pane; the ownership probe runs
/// under the per-workspace send lock, immediately before the paste.
async fn send_cli_text(
    State(deployment): State<DeploymentImpl>,
    axum::extract::Path(workspace_id): axum::extract::Path<Uuid>,
    Json(request): Json<SendCliTextRequest>,
) -> Result<Json<ApiResponse<SendCliTextResponse>>, ApiError> {
    send_text_to_cli_agent(&deployment.db().pool, workspace_id, &request.text)
        .await
        .map(|response| Json(ApiResponse::success(response)))
}

/// Type `text` into the workspace's live CLI agent pane and submit it.
/// `ApiError::Conflict` means no Claude/Codex agent is running there.
pub(crate) async fn send_text_to_cli_agent(
    pool: &SqlitePool,
    workspace_id: Uuid,
    text: &str,
) -> Result<SendCliTextResponse, ApiError> {
    let text = validate_cli_send_text(text).map_err(ApiError::BadRequest)?;

    let workspace = Workspace::find_by_id(pool, workspace_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Workspace not found".to_string()))?;
    let dir = PathBuf::from(workspace.container_ref.unwrap_or_default());
    let session = Session::find_for_cli(pool, workspace_id, None).await?;

    // The program the attach path launches comes first; the other known
    // agents follow because the pane keeps whichever agent launched it even
    // after the workspace's latest session switches executor.
    let expected = resolve_cli_agent_spec(session.as_ref(), None, None, &dir).program;
    let mut programs = vec![expected.as_str()];
    programs.extend(CLI_AGENT_PROGRAMS.iter().filter(|p| **p != expected));

    let no_agent = || {
        ApiError::Conflict(
            "No live Claude Code or Codex agent is running in this workspace's CLI pane"
                .to_string(),
        )
    };
    let target = locate_cli_tmux_target(workspace_id)
        .await
        .ok_or_else(no_agent)?;
    match send_cli_keys_to_live_agent(pool, &target, &programs, text)
        .await
        .map_err(|e| ApiError::Conflict(e.to_string()))?
    {
        None => Err(no_agent()),
        Some(CliSendResult::NotDelivered) => Err(ApiError::BadGateway(
            "tmux did not confirm the paste; check the CLI pane before sending again".to_string(),
        )),
        Some(result) => Ok(SendCliTextResponse {
            submitted: result == CliSendResult::Submitted,
        }),
    }
}

/// Grid of the pane a headless launch creates. The first browser attach
/// resizes it to the real terminal.
const HEADLESS_COLS: u16 = 160;
const HEADLESS_ROWS: u16 = 48;

/// Start the workspace's CLI agent with no browser attached, so a workspace
/// created through the API or MCP starts working at once. It is the terminal
/// attach without the WebSocket: same launch, same parked-prompt delivery,
/// then the PTY client detaches and the tmux session keeps running.
async fn launch_cli_agent(
    State(deployment): State<DeploymentImpl>,
    axum::extract::Path(workspace_id): axum::extract::Path<Uuid>,
) -> Result<Json<ApiResponse<LaunchCliAgentResponse>>, ApiError> {
    if !cli_tmux_available() {
        return Err(ApiError::Conflict(
            "tmux is not available, so no CLI agent can run".to_string(),
        ));
    }
    let workspace = Workspace::find_by_id(&deployment.db().pool, workspace_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Workspace not found".to_string()))?;
    let base_dir = workspace_dir(&workspace)?;
    let already_up = cli_tmux_session_exists_checked(workspace_id).await?;

    let spawned = spawn_cli_terminal(
        &deployment,
        &workspace,
        &base_dir,
        None,
        false,
        (HEADLESS_COLS, HEADLESS_ROWS),
    )
    .await?;
    let _ = deployment.pty().close_session(spawned.session_id).await;

    Ok(Json(ApiResponse::success(LaunchCliAgentResponse {
        launched: !already_up,
    })))
}

/// The visible text of the workspace's CLI pane, for callers that cannot
/// attach a terminal (MCP tools).
async fn get_cli_screen(
    axum::extract::Path(workspace_id): axum::extract::Path<Uuid>,
) -> Json<ApiResponse<CliScreenResponse>> {
    Json(ApiResponse::success(CliScreenResponse {
        screen: capture_cli_pane(workspace_id).await,
    }))
}

pub(super) fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route("/terminal/ws", get(terminal_ws))
        .route("/workspaces/{workspace_id}/cli/send", post(send_cli_text))
        .route(
            "/workspaces/{workspace_id}/cli/launch",
            post(launch_cli_agent),
        )
        .route("/workspaces/{workspace_id}/cli/screen", get(get_cli_screen))
}

#[cfg(test)]
mod tests {
    use db::models::session::Session;
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use super::{
        MAX_CLI_SEND_BYTES, cli_mcp_servers, codex_lean_mcp_args_for, lean_mcp_names,
        validate_cli_send_text,
    };

    #[test]
    fn cli_mcp_servers_keep_the_lean_set_plus_the_workspace_override() {
        let user = serde_json::json!({"mcpServers": {
            "slack": {"type": "http", "url": "https://mcp.slack.com/mcp"},
            "fff": {"command": "fff-mcp"},
            "gmail": {"command": "npx"},
            "playwright": {"command": "npx"},
        }});
        let names = |servers: serde_json::Map<String, serde_json::Value>| {
            let mut names: Vec<_> = servers.keys().cloned().collect();
            names.sort();
            names
        };
        let lean = lean_mcp_names(None).unwrap();
        assert_eq!(
            names(cli_mcp_servers(Some(&user), Some(&lean)).unwrap()),
            ["fff", "slack"]
        );
        let extra = lean_mcp_names(Some(&serde_json::json!(["playwright", "missing"]))).unwrap();
        let servers = cli_mcp_servers(Some(&user), Some(&extra)).unwrap();
        assert_eq!(servers["slack"], user["mcpServers"]["slack"]);
        assert_eq!(names(servers), ["fff", "playwright", "slack"]);
        assert!(lean_mcp_names(Some(&serde_json::json!("all"))).is_none());
        assert!(cli_mcp_servers(Some(&user), None).is_none());
        assert!(cli_mcp_servers(None, Some(&lean)).is_none());
    }

    #[test]
    fn codex_lean_args_disable_every_other_server() {
        let config: toml::Table = r#"
            [mcp_servers.slack]
            url = "https://mcp.slack.com/mcp"
            [mcp_servers.gmail]
            command = "npx"
            [mcp_servers.bettercoding]
            command = "bettercoding"
            [mcp_servers."odd.name"]
            command = "x"
        "#
        .parse()
        .unwrap();
        let lean = lean_mcp_names(None).unwrap();
        let disabled = |unrestricted| {
            let args = codex_lean_mcp_args_for(Some(&config), &lean, unrestricted);
            for pair in args.chunks(2) {
                assert_eq!(pair[0], "-c");
                pair[1].parse::<toml::Table>().unwrap();
            }
            args.into_iter().filter(|a| a != "-c").collect::<Vec<_>>()
        };
        assert_eq!(
            disabled(false),
            [
                "mcp_servers.bettercoding.enabled=false",
                "mcp_servers.gmail.enabled=false"
            ]
        );
        assert_eq!(disabled(true), ["mcp_servers.gmail.enabled=false"]);
        assert!(codex_lean_mcp_args_for(None, &lean, false).is_empty());
    }

    #[test]
    fn cli_send_rejects_empty_and_whitespace_text() {
        for empty in ["", " ", "\n\n", " \t \n "] {
            assert!(validate_cli_send_text(empty).is_err(), "{empty:?}");
        }
    }

    #[test]
    fn cli_send_caps_text_at_256_kib() {
        let at_cap = "x".repeat(MAX_CLI_SEND_BYTES);
        assert_eq!(validate_cli_send_text(&at_cap), Ok(at_cap.as_str()));
        assert!(validate_cli_send_text(&"x".repeat(MAX_CLI_SEND_BYTES + 1)).is_err());
        // The cap counts bytes, not chars.
        let multibyte = "é".repeat(MAX_CLI_SEND_BYTES / 2 + 1);
        assert!(validate_cli_send_text(&multibyte).is_err());
    }

    #[test]
    fn cli_send_refuses_control_characters_but_keeps_newlines_and_tabs() {
        // ESC [201~ would close the bracketed paste early.
        for bad in ["a\x1b[201~b", "a\rb", "a\x03", "a\u{9b}b"] {
            assert!(validate_cli_send_text(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            validate_cli_send_text("\n  line one\n\tline two ünï 🚀\n\n"),
            Ok("  line one\n\tline two ünï 🚀")
        );
    }

    /// Minimal in-memory slice of the `sessions` table — just the columns the
    /// parked-prompt primitives touch — so the CAS/park semantics that guard
    /// the "never destroy the prompt" invariant are exercised against real
    /// SQLite. (Lives here rather than in `crates/db` because the db crate
    /// has no async test runtime; this route module is the consumer whose
    /// correctness depends on these semantics.)
    async fn pool_with_session(id: Uuid, workspace_id: Uuid) -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE sessions (
                 id BLOB PRIMARY KEY,
                 workspace_id BLOB NOT NULL,
                 pending_cli_prompt TEXT,
                 created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO sessions (id, workspace_id) VALUES ($1, $2)")
            .bind(id)
            .bind(workspace_id)
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn clear_pending_cli_prompt_is_compare_and_swap() {
        let sid = Uuid::new_v4();
        let pool = pool_with_session(sid, Uuid::new_v4()).await;

        // Park, then clear with the exact delivered value: clears.
        Session::set_pending_cli_prompt(&pool, sid, "deliver me")
            .await
            .unwrap();
        assert!(
            Session::clear_pending_cli_prompt(&pool, sid, "deliver me")
                .await
                .unwrap(),
            "matching clear must succeed"
        );
        assert_eq!(
            Session::peek_pending_cli_prompt(&pool, sid).await.unwrap(),
            None
        );

        // A NEWER prompt parked mid-confirmation survives the older
        // delivery's clear (the CAS misses) — the invariant this method
        // exists for.
        Session::set_pending_cli_prompt(&pool, sid, "newer prompt")
            .await
            .unwrap();
        assert!(
            !Session::clear_pending_cli_prompt(&pool, sid, "deliver me")
                .await
                .unwrap(),
            "stale clear must be superseded"
        );
        assert_eq!(
            Session::peek_pending_cli_prompt(&pool, sid).await.unwrap(),
            Some("newer prompt".to_string())
        );

        // Clearing an empty slot is a no-op, not an error.
        Session::clear_pending_cli_prompt(&pool, sid, "newer prompt")
            .await
            .unwrap();
        assert!(
            !Session::clear_pending_cli_prompt(&pool, sid, "newer prompt")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn repark_only_fills_an_empty_slot() {
        let sid = Uuid::new_v4();
        let pool = pool_with_session(sid, Uuid::new_v4()).await;

        // Empty slot: the wake-up parks.
        assert!(
            Session::set_pending_cli_prompt_if_empty(&pool, sid, "continue")
                .await
                .unwrap()
        );
        // Occupied slot: a wake-up must never overwrite a parked prompt.
        assert!(
            !Session::set_pending_cli_prompt_if_empty(&pool, sid, "boilerplate")
                .await
                .unwrap()
        );
        assert_eq!(
            Session::peek_pending_cli_prompt(&pool, sid).await.unwrap(),
            Some("continue".to_string())
        );
    }

    #[tokio::test]
    async fn workspace_peek_finds_prompt_on_any_session_row() {
        let workspace_id = Uuid::new_v4();
        let older = Uuid::new_v4();
        let pool = pool_with_session(older, workspace_id).await;
        // A second, newer session in the same workspace (distinct created_at
        // ordering via explicit timestamps).
        let newer = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO sessions (id, workspace_id, created_at)
             VALUES ($1, $2, datetime('now', '+1 hour'))",
        )
        .bind(newer)
        .bind(workspace_id)
        .execute(&pool)
        .await
        .unwrap();

        // Prompt parked on the OLDER (e.g. CLI-first) session is still found
        // when the attach resolved the newer session.
        Session::set_pending_cli_prompt(&pool, older, "parked on older")
            .await
            .unwrap();
        assert_eq!(
            Session::peek_pending_cli_prompt_for_workspace(&pool, workspace_id)
                .await
                .unwrap(),
            Some((older, "parked on older".to_string()))
        );

        // With prompts on both rows, the newest session's wins (loop re-parks
        // land on the latest session).
        Session::set_pending_cli_prompt(&pool, newer, "parked on newer")
            .await
            .unwrap();
        assert_eq!(
            Session::peek_pending_cli_prompt_for_workspace(&pool, workspace_id)
                .await
                .unwrap(),
            Some((newer, "parked on newer".to_string()))
        );

        // Foreign workspaces see nothing.
        assert_eq!(
            Session::peek_pending_cli_prompt_for_workspace(&pool, Uuid::new_v4())
                .await
                .unwrap(),
            None
        );
    }
}

#[cfg(test)]
mod tripwire_tests {
    use uuid::Uuid;

    use super::{
        AttachInputTripwire, HOOK_EVENTS, codex_hook_args_for, codex_mcp_args_for, hex_dump,
        hook_report_command, runs_unrestricted,
    };

    fn tripwire() -> AttachInputTripwire {
        AttachInputTripwire::new("bc_test".to_string(), Uuid::new_v4())
    }

    /// The byte budget: chunks are truncated to the remaining budget, and a
    /// spent budget stops all logging state changes.
    #[test]
    fn observe_caps_logged_bytes_at_budget() {
        let mut tw = tripwire();
        tw.observe(&[0x0a; 100]);
        assert_eq!(tw.bytes_logged, 100);
        // 100 + 100 crosses the 128 cap: only the remainder is taken.
        tw.observe(&[0x0a; 100]);
        assert_eq!(tw.bytes_logged, AttachInputTripwire::MAX_BYTES);
        // Spent: further input changes nothing.
        tw.observe(&[0x0a; 4]);
        assert_eq!(tw.bytes_logged, AttachInputTripwire::MAX_BYTES);
        // Empty chunks never count.
        let mut fresh = tripwire();
        fresh.observe(&[]);
        assert_eq!(fresh.bytes_logged, 0);
    }

    /// The time window: the first out-of-window observation latches the
    /// budget as spent (behavior-identical to checking the clock forever,
    /// but cheaper), and nothing is logged after expiry.
    #[test]
    fn observe_latches_after_window_expires() {
        let mut tw = tripwire();
        // Simulate an expired window without sleeping.
        tw.started = std::time::Instant::now() - (AttachInputTripwire::WINDOW * 2);
        tw.observe(&[0x0a]);
        assert_eq!(
            tw.bytes_logged,
            AttachInputTripwire::MAX_BYTES,
            "expiry must latch the budget as spent without logging"
        );
    }

    /// Codex reads each `-c key=value` as a line of TOML: every hook override
    /// must parse, and must carry the report command byte for byte.
    #[test]
    fn codex_hook_overrides_are_valid_toml_carrying_the_report_command() {
        let id = Uuid::nil();
        let args = codex_hook_args_for(4111, id);
        assert_eq!(args[0], "--dangerously-bypass-hook-trust");
        let overrides: Vec<&String> = args[1..].iter().skip(1).step_by(2).collect();
        let registered: Vec<&str> = super::CODEX_HOOK_EVENTS
            .iter()
            .chain(super::CODEX_ONLY_HOOK_EVENTS)
            .copied()
            .collect();
        assert_eq!(overrides.len(), registered.len());
        for value in overrides {
            let doc: toml::Table = value.parse().unwrap_or_else(|e| panic!("{value}: {e}"));
            let (event, groups) = doc["hooks"].as_table().unwrap().iter().next().unwrap();
            assert!(registered.contains(&event.as_str()));
            let hook = &groups[0]["hooks"][0];
            assert_eq!(hook["type"].as_str(), Some("command"));
            assert_eq!(
                hook["command"].as_str(),
                Some(hook_report_command(4111, id).as_str())
            );
        }
    }

    /// A sandboxed agent never gets the MCP: its tools reach unrestricted
    /// agents in other workspaces.
    #[test]
    fn only_unrestricted_agents_get_the_mcp() {
        use executors::executors::cli::CliLaunchSpec;
        let spec = |program: &str, args: &[&str]| {
            CliLaunchSpec::new(program, args.iter().map(|a| a.to_string()).collect())
        };
        assert!(runs_unrestricted(&spec(
            "claude",
            &["--dangerously-skip-permissions"]
        )));
        assert!(!runs_unrestricted(&spec("claude", &["--model", "opus"])));
        assert!(runs_unrestricted(&spec(
            "codex",
            &["-s", "danger-full-access"]
        )));
        assert!(!runs_unrestricted(&spec(
            "codex",
            &["-s", "workspace-write"]
        )));
        assert!(!runs_unrestricted(&spec("gemini", &[])));
    }

    /// The MCP overrides merge into one valid `mcp_servers.bettercoding` table
    /// that starts this binary's `mcp` subcommand against this server.
    #[test]
    fn codex_mcp_overrides_are_valid_toml() {
        let args = codex_mcp_args_for("/opt/bc dir/server \"x\"", "http://127.0.0.1:4111");
        let mut server = toml::Table::new();
        for pair in args.chunks(2) {
            assert_eq!(pair[0], "-c");
            let doc: toml::Table = pair[1]
                .parse()
                .unwrap_or_else(|e| panic!("{}: {e}", pair[1]));
            let entry = &doc["mcp_servers"]["bettercoding"];
            server.extend(entry.as_table().unwrap().clone());
        }
        assert_eq!(server["command"].as_str(), Some("/opt/bc dir/server \"x\""));
        assert_eq!(server["args"][0].as_str(), Some("mcp"));
        assert_eq!(
            server["env"]["BC_BACKEND_URL"].as_str(),
            Some("http://127.0.0.1:4111")
        );
    }

    #[test]
    fn hex_dump_shows_control_bytes_and_masks_printables() {
        // The \n+EOT injection signature must stay fully visible…
        assert_eq!(hex_dump(&[0x0a, 0x04]), "0a 04");
        // …while printable payload (keystrokes, secrets) is masked to `..`,
        // keeping only count and position.
        assert_eq!(hex_dump(b"hi"), ".. ..");
        assert_eq!(hex_dump(&[0x1b, b'[', b'A']), "1b .. ..");
        assert_eq!(hex_dump(&[0x7f]), "7f");
        assert_eq!(hex_dump(&[]), "");
    }

    /// Locks the redaction invariant over the whole byte range: ONLY the C0
    /// controls (0x00-0x1f) and DEL (0x7f) may appear verbatim; every
    /// printable AND high/UTF-8 byte (0x20-0x7e, 0x80-0xff) must be masked so
    /// no keystroke content can ever reach the logs.
    #[test]
    fn hex_dump_masks_every_non_control_byte() {
        for byte in 0x00u8..=0xff {
            let dumped = hex_dump(&[byte]);
            if byte < 0x20 || byte == 0x7f {
                assert_eq!(dumped, format!("{byte:02x}"), "control byte {byte:#04x}");
            } else {
                assert_eq!(dumped, "..", "byte {byte:#04x} must be masked");
            }
        }
    }
    /// The one property that matters about this hook: Claude reads a non-zero
    /// exit as a BLOCK on the turn. Point it at a port nothing is listening on
    /// — the worst realistic case, a server that went away under a live pane —
    /// and it must still succeed and print nothing.
    #[test]
    fn the_report_hook_cannot_block_a_turn() {
        // Port 1 is privileged and unbound: connecting to it always fails.
        let command = hook_report_command(1, Uuid::nil());
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .expect("sh is available");
        assert!(
            output.status.success(),
            "hook exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "hook must not emit into the turn");
    }

    #[test]
    fn every_reduced_event_is_registered() {
        // The reducer and the registration read the same list, so an event can
        // never be understood but not subscribed to.
        assert_eq!(HOOK_EVENTS.len(), 15);
    }
}
