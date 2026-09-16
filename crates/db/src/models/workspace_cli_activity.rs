use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use ts_rs::TS;
use uuid::Uuid;

/// Live state of a workspace's CLI-mode tmux claude session.
///
/// Maintained by the local deployment's tmux poller (`CliActivityMonitor`):
/// `running` while the pane is producing output, `attention` once a run went
/// quiet while no client was attached (claude finished while the user was on
/// another workspace), `idle` otherwise. Rows are upserted only on state
/// transitions so the SQLite update hook broadcasts a workspace patch exactly
/// when the sidebar bucket should move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliActivityState {
    Idle,
    Running,
    Attention,
}

impl CliActivityState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CliActivityState::Idle => "idle",
            CliActivityState::Running => "running",
            CliActivityState::Attention => "attention",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "running" => CliActivityState::Running,
            "attention" => CliActivityState::Attention,
            _ => CliActivityState::Idle,
        }
    }
}

/// What a CLI-mode Claude Code session reports it is doing, reduced from its
/// own hook events (see [`reduce_hook`]).
///
/// Distinct from [`CliActivityState`], which stays the coarse sidebar bucket
/// the tmux poller can also produce for agents that report nothing. A phase is
/// the agent's own account of itself; a bucket is what the sidebar does with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CliPhase {
    /// Session started, nothing asked of it yet.
    Ready,
    /// A turn is in flight.
    Working,
    /// Blocked on an answer from the user (AskUserQuestion / elicitation).
    Question,
    /// Blocked on a tool-permission decision.
    Approval,
    /// Claude's own idle notification: it has been waiting a while.
    Attention,
    /// The turn ended. `tasks`/`crons` say whether anything is still armed.
    Stopped,
    /// Compacting the context window.
    Compacting,
    /// The last tool call failed.
    ToolFailed,
    /// The turn could not end because the account is rate limited.
    RateLimit,
    /// The turn could not end because of an API error.
    Error,
    /// The session exited.
    Ended,
}

impl CliPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            CliPhase::Ready => "ready",
            CliPhase::Working => "working",
            CliPhase::Question => "question",
            CliPhase::Approval => "approval",
            CliPhase::Attention => "attention",
            CliPhase::Stopped => "stopped",
            CliPhase::Compacting => "compacting",
            CliPhase::ToolFailed => "tool_failed",
            CliPhase::RateLimit => "rate_limit",
            CliPhase::Error => "error",
            CliPhase::Ended => "ended",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ready" => CliPhase::Ready,
            "working" => CliPhase::Working,
            "question" => CliPhase::Question,
            "approval" => CliPhase::Approval,
            "attention" => CliPhase::Attention,
            "stopped" => CliPhase::Stopped,
            "compacting" => CliPhase::Compacting,
            "tool_failed" => CliPhase::ToolFailed,
            "rate_limit" => CliPhase::RateLimit,
            "error" => CliPhase::Error,
            "ended" => CliPhase::Ended,
            _ => return None,
        })
    }

    /// The sidebar bucket this phase implies.
    ///
    /// `Stopped` maps to `Attention` rather than `Idle`: a finished turn is the
    /// thing the user has to come back to. The poller clears it as before, the
    /// moment a terminal attaches.
    pub fn bucket(&self) -> CliActivityState {
        match self {
            CliPhase::Working | CliPhase::Compacting => CliActivityState::Running,
            CliPhase::Question
            | CliPhase::Approval
            | CliPhase::Attention
            | CliPhase::Stopped
            | CliPhase::ToolFailed
            | CliPhase::RateLimit
            | CliPhase::Error => CliActivityState::Attention,
            CliPhase::Ready | CliPhase::Ended => CliActivityState::Idle,
        }
    }
}

/// One workspace's hook-reported CLI session state — the reduced form of every
/// hook event the session has sent so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliHookState {
    /// The agent's own session id, which `--resume` and the rename integration
    /// both key on.
    pub agent_session_id: String,
    /// Absolute path of the session transcript, straight from the payload. The
    /// rename integration derives claude's title sidecar from it.
    pub transcript_path: Option<String>,
    pub phase: CliPhase,
    /// Background tasks still armed at the last turn end. `None` means unknown
    /// (the payload carried no list) — never an authoritative zero, and never a
    /// stale count from an earlier turn.
    pub tasks: Option<i64>,
    pub crons: Option<i64>,
    /// Monotonic nanosecond stamp minted by the reporting hook. Reports that do
    /// not advance it are dropped, so events that overtake each other in flight
    /// cannot rewind the phase.
    pub seq: i64,
}

/// Hook events a CLI-mode session reports. Anything else is ignored outright,
/// so a future event cannot be mistaken for one of these.
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "Stop",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "StopFailure",
    "SessionEnd",
    "Elicitation",
    "ElicitationResult",
];

fn payload_str<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(|v| v.as_str())
}

/// Reduce one Claude Code hook payload into the workspace's new session state,
/// or `None` when the event must not change anything.
///
/// Pure: every decision is a function of the previous state, the payload and
/// the caller's sequence number, so the whole table is unit-testable without a
/// tmux pane, a socket or a database.
pub fn reduce_hook(
    previous: Option<&CliHookState>,
    payload: &serde_json::Value,
    seq: i64,
) -> Option<CliHookState> {
    let event = payload_str(payload, "hook_event_name")?;
    if !HOOK_EVENTS.contains(&event) {
        return None;
    }
    let session_id = payload_str(payload, "session_id").filter(|s| !s.is_empty())?;

    // A subagent's events describe the subagent, not the pane. Only its
    // terminal event is useful, and only for refreshing background counts.
    let subagent = payload
        .get("agent_id")
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
    if subagent && event != "SubagentStop" {
        return None;
    }
    // Cursor ships a fork of the hook protocol whose payloads mean other things.
    if payload.get("cursor_version").is_some() {
        return None;
    }
    if previous.is_some_and(|p| seq <= p.seq) {
        return None;
    }
    // A different session in the same pane only takes over by starting.
    if previous.is_some_and(|p| p.agent_session_id != session_id) && event != "SessionStart" {
        return None;
    }
    if previous.is_some_and(|p| p.phase == CliPhase::Ended) && event != "SessionStart" {
        return None;
    }

    let fresh = CliHookState {
        agent_session_id: session_id.to_string(),
        transcript_path: None,
        phase: CliPhase::Ready,
        tasks: None,
        crons: None,
        seq,
    };
    let mut state = match previous {
        Some(previous) if event != "SessionStart" => previous.clone(),
        _ => fresh,
    };
    state.seq = seq;
    // Every payload carries it; keep the last known one if one ever omits it.
    if let Some(path) = payload_str(payload, "transcript_path").filter(|s| !s.is_empty()) {
        state.transcript_path = Some(path.to_string());
    }

    // Only the parent's own turn-end events carry authoritative registries.
    if event == "Stop" || event == "SubagentStop" {
        let len = |key: &str| {
            payload
                .get(key)
                .and_then(|v| v.as_array())
                .map(|items| items.len() as i64)
        };
        state.tasks = len("background_tasks");
        state.crons = len("session_crons");
    }

    state.phase = match event {
        "SessionStart" => CliPhase::Ready,
        "UserPromptSubmit" | "PostToolUse" | "ElicitationResult" | "PostCompact" => {
            CliPhase::Working
        }
        "PreToolUse" => match payload_str(payload, "tool_name") {
            Some("AskUserQuestion") => CliPhase::Question,
            _ => CliPhase::Working,
        },
        "PostToolUseFailure" => CliPhase::ToolFailed,
        "PermissionRequest" => CliPhase::Approval,
        "Elicitation" => CliPhase::Question,
        "Notification" => match payload_str(payload, "notification_type") {
            Some("permission_prompt") => CliPhase::Approval,
            // Claude sends an idle notification AFTER Stop. Letting it through
            // would overwrite a turn-ended-with-background-work observation
            // with a generic "waiting", so the known phase wins.
            Some("idle_prompt") => match state.phase {
                CliPhase::Stopped | CliPhase::Error | CliPhase::RateLimit => state.phase,
                _ => CliPhase::Attention,
            },
            _ => return None,
        },
        "Stop" => CliPhase::Stopped,
        "PreCompact" => CliPhase::Compacting,
        "StopFailure" => match payload_str(payload, "error") {
            Some("rate_limit") => CliPhase::RateLimit,
            _ => CliPhase::Error,
        },
        "SessionEnd" => CliPhase::Ended,
        // SubagentStop refreshed the counts above without taking the pane's
        // foreground phase: a subagent finishing mid-turn is not a turn end.
        _ => state.phase,
    };
    Some(state)
}

#[derive(Debug, Clone)]
pub struct WorkspaceCliActivity {
    pub workspace_id: Uuid,
    pub state: CliActivityState,
    pub updated_at: DateTime<Utc>,
    /// The agent's own account of what it is doing. `None` for agents that
    /// report nothing (codex, gemini, a claude too old for the hook).
    pub phase: Option<CliPhase>,
    pub hook: Option<CliHookState>,
    /// When the phase was last reported. `None` for a row the poller alone
    /// has ever written.
    pub hook_at: Option<DateTime<Utc>>,
}

/// How long a reported phase keeps describing the pane. Ported from the
/// reference implementation's one-hour metadata TTL: a session that has not
/// reported for an hour is no longer making a claim about itself, so the tmux
/// poller's guess takes over again.
pub const HOOK_PHASE_TTL_SECS: i64 = 3600;

impl WorkspaceCliActivity {
    /// The agent's own phase, but only while its last report is recent enough
    /// to still be about the current pane. Every consumer of `phase` goes
    /// through this, so "how stale is too stale" is decided in one place.
    pub fn fresh_phase(&self, now: DateTime<Utc>) -> Option<CliPhase> {
        let at = self.hook_at?;
        (now.signed_duration_since(at).num_seconds() < HOOK_PHASE_TTL_SECS)
            .then_some(self.phase)
            .flatten()
    }

    /// Set a workspace's CLI activity state. No-op write avoidance is the
    /// caller's job (the monitor only calls this on transitions).
    pub async fn upsert(
        pool: &SqlitePool,
        workspace_id: Uuid,
        state: CliActivityState,
    ) -> Result<(), sqlx::Error> {
        let state = state.as_str();
        sqlx::query!(
            r#"INSERT INTO workspace_cli_activity (workspace_id, state, updated_at)
               VALUES ($1, $2, datetime('now', 'subsec'))
               ON CONFLICT(workspace_id) DO UPDATE SET
                 state = excluded.state,
                 updated_at = excluded.updated_at"#,
            workspace_id,
            state
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Record a hook-reduced state, refreshing `state` from the phase's bucket
    /// so the coarse sidebar projection and the phase can never disagree.
    /// Stale `seq` is dropped silently: events that overtake each other in
    /// flight must not rewind the phase.
    pub async fn upsert_hook(
        pool: &SqlitePool,
        workspace_id: Uuid,
        hook: &CliHookState,
        at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let state = hook.phase.bucket().as_str();
        let phase = hook.phase.as_str();
        let transcript_path = hook.transcript_path.as_deref();
        sqlx::query!(
            r#"INSERT INTO workspace_cli_activity
                 (workspace_id, state, updated_at, phase, tasks, crons,
                  agent_session_id, transcript_path, seq, hook_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
               ON CONFLICT(workspace_id) DO UPDATE SET
                 state = excluded.state,
                 updated_at = excluded.updated_at,
                 phase = excluded.phase,
                 tasks = excluded.tasks,
                 crons = excluded.crons,
                 agent_session_id = excluded.agent_session_id,
                 transcript_path = excluded.transcript_path,
                 seq = excluded.seq,
                 hook_at = excluded.hook_at
               WHERE excluded.seq >= workspace_cli_activity.seq"#,
            workspace_id,
            state,
            at,
            phase,
            hook.tasks,
            hook.crons,
            hook.agent_session_id,
            transcript_path,
            hook.seq,
            at,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Current states for all workspaces that have a row.
    pub async fn find_all(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT
                 workspace_id as "workspace_id!: Uuid",
                 state,
                 updated_at as "updated_at!: DateTime<Utc>",
                 phase,
                 tasks,
                 crons,
                 agent_session_id,
                 transcript_path,
                 seq,
                 hook_at as "hook_at?: DateTime<Utc>"
               FROM workspace_cli_activity"#
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| Self {
                workspace_id: r.workspace_id,
                state: CliActivityState::parse(&r.state),
                updated_at: r.updated_at,
                phase: r.phase.as_deref().and_then(CliPhase::parse),
                hook: r.agent_session_id.map(|agent_session_id| CliHookState {
                    agent_session_id,
                    transcript_path: r.transcript_path,
                    phase: r
                        .phase
                        .as_deref()
                        .and_then(CliPhase::parse)
                        .unwrap_or(CliPhase::Ready),
                    tasks: r.tasks,
                    crons: r.crons,
                    seq: r.seq,
                }),
                hook_at: r.hook_at,
            })
            .collect())
    }

    /// Workspaces whose CLI session finished while unattended (for the
    /// "Needs Attention" sidebar bucket), filtered by archived status.
    pub async fn find_workspaces_needing_attention(
        pool: &SqlitePool,
        archived: bool,
    ) -> Result<std::collections::HashSet<Uuid>, sqlx::Error> {
        let result: Vec<Uuid> = sqlx::query_scalar!(
            r#"SELECT ca.workspace_id as "workspace_id!: Uuid"
               FROM workspace_cli_activity ca
               JOIN workspaces w ON ca.workspace_id = w.id
               WHERE ca.state = 'attention' AND w.archived = $1"#,
            archived
        )
        .fetch_all(pool)
        .await?;

        Ok(result.into_iter().collect())
    }

    /// One workspace's row, for reducing the next hook report against the
    /// state the previous one left.
    pub async fn find_by_workspace_id(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT
                 workspace_id as "workspace_id!: Uuid",
                 state,
                 updated_at as "updated_at!: DateTime<Utc>",
                 phase,
                 tasks,
                 crons,
                 agent_session_id,
                 transcript_path,
                 seq,
                 hook_at as "hook_at?: DateTime<Utc>"
               FROM workspace_cli_activity
               WHERE workspace_id = $1"#,
            workspace_id
        )
        .fetch_optional(pool)
        .await?;

        Ok(row.map(|r| Self {
            workspace_id: r.workspace_id,
            state: CliActivityState::parse(&r.state),
            updated_at: r.updated_at,
            phase: r.phase.as_deref().and_then(CliPhase::parse),
            hook: r.agent_session_id.map(|agent_session_id| CliHookState {
                agent_session_id,
                transcript_path: r.transcript_path,
                phase: r
                    .phase
                    .as_deref()
                    .and_then(CliPhase::parse)
                    .unwrap_or(CliPhase::Ready),
                tasks: r.tasks,
                crons: r.crons,
                seq: r.seq,
            }),
            hook_at: r.hook_at,
        }))
    }

    /// Row lookup for the SQLite update hook (hooks only get a rowid).
    pub async fn find_by_rowid(pool: &SqlitePool, rowid: i64) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT
                 workspace_id as "workspace_id!: Uuid",
                 state,
                 updated_at as "updated_at!: DateTime<Utc>",
                 phase,
                 tasks,
                 crons,
                 agent_session_id,
                 transcript_path,
                 seq,
                 hook_at as "hook_at?: DateTime<Utc>"
               FROM workspace_cli_activity
               WHERE rowid = $1"#,
            rowid
        )
        .fetch_optional(pool)
        .await?;

        Ok(row.map(|r| Self {
            workspace_id: r.workspace_id,
            state: CliActivityState::parse(&r.state),
            updated_at: r.updated_at,
            phase: r.phase.as_deref().and_then(CliPhase::parse),
            hook: r.agent_session_id.map(|agent_session_id| CliHookState {
                agent_session_id,
                transcript_path: r.transcript_path,
                phase: r
                    .phase
                    .as_deref()
                    .and_then(CliPhase::parse)
                    .unwrap_or(CliPhase::Ready),
                tasks: r.tasks,
                crons: r.crons,
                seq: r.seq,
            }),
            hook_at: r.hook_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event(event: &str) -> serde_json::Value {
        json!({"session_id": "s1", "hook_event_name": event})
    }

    fn with_session(payload: serde_json::Value, session_id: &str) -> serde_json::Value {
        let mut payload = payload;
        payload["session_id"] = json!(session_id);
        payload
    }

    /// Reduce a sequence of payloads from a fresh state.
    fn run(events: &[serde_json::Value]) -> Option<CliHookState> {
        let mut state: Option<CliHookState> = None;
        for (i, payload) in events.iter().enumerate() {
            state = reduce_hook(state.as_ref(), payload, (i + 1) as i64);
        }
        state
    }

    #[test]
    fn session_start_is_ready_with_null_counts() {
        let state = run(&[event("SessionStart")]).unwrap();
        assert_eq!(state.phase, CliPhase::Ready);
        assert_eq!(state.agent_session_id, "s1");
        assert_eq!(state.tasks, None);
        assert_eq!(state.crons, None);
        assert_eq!(state.transcript_path, None);
    }

    #[test]
    fn unknown_event_is_dropped() {
        assert!(run(&[event("SomethingNew")]).is_none());
    }

    #[test]
    fn missing_or_empty_session_id_is_dropped() {
        assert!(run(&[json!({"hook_event_name": "SessionStart"})]).is_none());
        assert!(
            run(&[json!({
                "hook_event_name": "SessionStart",
                "session_id": ""
            })])
            .is_none()
        );
    }

    #[test]
    fn phase_table_covers_every_registered_event() {
        let cases: &[(&str, CliPhase)] = &[
            ("UserPromptSubmit", CliPhase::Working),
            ("PostToolUse", CliPhase::Working),
            ("ElicitationResult", CliPhase::Working),
            ("PostCompact", CliPhase::Working),
            ("PreToolUse", CliPhase::Working),
            ("PostToolUseFailure", CliPhase::ToolFailed),
            ("PermissionRequest", CliPhase::Approval),
            ("Elicitation", CliPhase::Question),
            ("Stop", CliPhase::Stopped),
            ("PreCompact", CliPhase::Compacting),
            ("SessionEnd", CliPhase::Ended),
        ];
        for (name, expected) in cases {
            let state = run(&[event("SessionStart"), event(name)]).unwrap();
            assert_eq!(state.phase, *expected, "event {name}");
        }
    }

    #[test]
    fn ask_user_question_tool_is_question_not_working() {
        let payload = json!({
            "session_id": "s1",
            "hook_event_name": "PreToolUse",
            "tool_name": "AskUserQuestion"
        });
        let state = run(&[event("SessionStart"), payload]).unwrap();
        assert_eq!(state.phase, CliPhase::Question);
    }

    #[test]
    fn stopfailure_splits_rate_limit_from_other_errors() {
        let rate = with_session(
            json!({
                "hook_event_name": "StopFailure",
                "error": "rate_limit"
            }),
            "s1",
        );
        let state = run(&[event("SessionStart"), rate]).unwrap();
        assert_eq!(state.phase, CliPhase::RateLimit);

        let other = with_session(
            json!({
                "hook_event_name": "StopFailure",
                "error": "overloaded"
            }),
            "s1",
        );
        let state = run(&[event("SessionStart"), other]).unwrap();
        assert_eq!(state.phase, CliPhase::Error);
    }

    #[test]
    fn notification_permission_prompt_is_approval_idle_is_attention() {
        let permission = with_session(
            json!({
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt"
            }),
            "s1",
        );
        let state = run(&[event("SessionStart"), permission]).unwrap();
        assert_eq!(state.phase, CliPhase::Approval);

        let idle = with_session(
            json!({
                "hook_event_name": "Notification",
                "notification_type": "idle_prompt"
            }),
            "s1",
        );
        let state = run(&[event("SessionStart"), idle]).unwrap();
        assert_eq!(state.phase, CliPhase::Attention);
    }

    #[test]
    fn unknown_notification_type_is_dropped() {
        let payload = with_session(
            json!({
                "hook_event_name": "Notification",
                "notification_type": "something_else"
            }),
            "s1",
        );
        assert!(run(&[event("SessionStart"), payload]).is_none());
    }

    #[test]
    fn stale_seq_is_dropped() {
        let first = run(&[event("SessionStart")]).unwrap();
        // An equal seq cannot rewind; only a strictly newer one applies.
        assert!(reduce_hook(Some(&first), &event("UserPromptSubmit"), first.seq).is_none());
        assert!(reduce_hook(Some(&first), &event("UserPromptSubmit"), first.seq - 1).is_none());
        assert!(reduce_hook(Some(&first), &event("UserPromptSubmit"), first.seq + 1).is_some());
    }

    #[test]
    fn subagent_events_are_dropped_except_subagent_stop() {
        let payload = with_session(
            json!({"hook_event_name": "PostToolUse", "agent_id": "child"}),
            "s1",
        );
        assert!(run(&[event("SessionStart"), payload]).is_none());
    }

    #[test]
    fn cursor_payloads_are_dropped() {
        let payload = with_session(
            json!({"hook_event_name": "SessionStart", "cursor_version": "1.0"}),
            "s1",
        );
        assert!(run(&[payload]).is_none());
    }

    #[test]
    fn different_session_only_takes_over_by_starting() {
        let state = run(&[event("SessionStart"), event("UserPromptSubmit")]).unwrap();
        assert_eq!(state.phase, CliPhase::Working);
        // A foreign session's events must not touch the pane's state…
        let foreign = with_session(event("Stop"), "s2");
        assert!(reduce_hook(Some(&state), &foreign, state.seq + 1).is_none());
        // …but its SessionStart does take the pane over.
        let takeover = with_session(event("SessionStart"), "s2");
        let state = reduce_hook(Some(&state), &takeover, state.seq + 1).unwrap();
        assert_eq!(state.agent_session_id, "s2");
        assert_eq!(state.phase, CliPhase::Ready);
    }

    #[test]
    fn events_after_ended_are_dropped_except_session_start() {
        let state = run(&[event("SessionStart"), event("SessionEnd")]).unwrap();
        assert_eq!(state.phase, CliPhase::Ended);
        assert!(reduce_hook(Some(&state), &event("UserPromptSubmit"), state.seq + 1).is_none());
        let state = reduce_hook(Some(&state), &event("SessionStart"), state.seq + 1).unwrap();
        assert_eq!(state.phase, CliPhase::Ready);
    }

    #[test]
    fn stop_stores_counts_and_missing_lists_mean_unknown() {
        let stop = with_session(
            json!({
                "hook_event_name": "Stop",
                "background_tasks": [{}, {}],
                "session_crons": []
            }),
            "s1",
        );
        let state = run(&[event("SessionStart"), stop.clone()]).unwrap();
        assert_eq!(state.phase, CliPhase::Stopped);
        assert_eq!(state.tasks, Some(2));
        assert_eq!(state.crons, Some(0));

        // A Stop with no registries stores unknown, never an authoritative
        // zero and never the stale count from the previous turn.
        let bare = event("Stop");
        let state = run(&[event("SessionStart"), stop.clone(), bare]).unwrap();
        assert_eq!(state.tasks, None);
        assert_eq!(state.crons, None);
    }

    #[test]
    fn subagent_stop_refreshes_counts_without_changing_phase() {
        let working = event("UserPromptSubmit");
        let stop = with_session(
            json!({
                "hook_event_name": "Stop",
                "background_tasks": [{}],
                "session_crons": [{}]
            }),
            "s1",
        );
        let subagent = with_session(
            json!({
                "hook_event_name": "SubagentStop",
                "agent_id": "child",
                "background_tasks": [],
                "session_crons": []
            }),
            "s1",
        );
        let state = run(&[event("SessionStart"), working, stop, subagent]).unwrap();
        // Still mid-turn as far as the pane is concerned…
        assert_eq!(state.phase, CliPhase::Stopped);
        // …but the counts refreshed to the authoritative zero.
        assert_eq!(state.tasks, Some(0));
        assert_eq!(state.crons, Some(0));
    }

    #[test]
    fn subagent_stop_mid_turn_does_not_flip_phase_to_turn_ended() {
        let subagent = with_session(
            json!({
                "hook_event_name": "SubagentStop",
                "agent_id": "child",
                "background_tasks": [{}, {}],
                "session_crons": []
            }),
            "s1",
        );
        let state = run(&[event("SessionStart"), event("UserPromptSubmit"), subagent]).unwrap();
        // A subagent finishing mid-turn is not a turn end.
        assert_eq!(state.phase, CliPhase::Working);
        assert_eq!(state.tasks, Some(2));
        assert_eq!(state.crons, Some(0));
    }

    #[test]
    fn idle_notification_preserves_stopped_error_and_rate_limit() {
        let idle = with_session(
            json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"}),
            "s1",
        );
        for phase in [CliPhase::Stopped, CliPhase::Error, CliPhase::RateLimit] {
            let state = CliHookState {
                agent_session_id: "s1".to_string(),
                transcript_path: None,
                phase,
                tasks: Some(1),
                crons: None,
                seq: 1,
            };
            let state = reduce_hook(Some(&state), &idle, 2).unwrap();
            assert_eq!(state.phase, phase, "phase {phase:?}");
        }
        // Any other phase gives way to attention.
        for phase in [CliPhase::Ready, CliPhase::Working, CliPhase::Approval] {
            let state = CliHookState {
                agent_session_id: "s1".to_string(),
                transcript_path: None,
                phase,
                tasks: None,
                crons: None,
                seq: 1,
            };
            let state = reduce_hook(Some(&state), &idle, 2).unwrap();
            assert_eq!(state.phase, CliPhase::Attention, "phase {phase:?}");
        }
    }

    #[test]
    fn transcript_path_is_kept_from_the_last_payload_that_carried_it() {
        let with_path = with_session(
            json!({
                "hook_event_name": "SessionStart",
                "transcript_path": "/tmp/s1.jsonl"
            }),
            "s1",
        );
        let state = run(&[with_path, event("UserPromptSubmit")]).unwrap();
        assert_eq!(state.transcript_path.as_deref(), Some("/tmp/s1.jsonl"));
    }

    #[test]
    fn first_event_without_session_start_still_seeds_a_state() {
        // A server restart can lose the stored row between two hook events;
        // the reducer must not depend on having seen SessionStart first.
        let state = run(&[event("UserPromptSubmit")]).unwrap();
        assert_eq!(state.phase, CliPhase::Working);
        assert_eq!(state.agent_session_id, "s1");
    }

    fn row(phase: Option<CliPhase>, hook_at: Option<DateTime<Utc>>) -> WorkspaceCliActivity {
        WorkspaceCliActivity {
            workspace_id: Uuid::nil(),
            state: CliActivityState::Idle,
            updated_at: Utc::now(),
            phase,
            hook: None,
            hook_at,
        }
    }

    #[test]
    fn a_phase_stops_describing_the_pane_once_its_report_ages_out() {
        let now = Utc::now();
        let ago = |secs: i64| Some(now - chrono::Duration::seconds(secs));
        assert_eq!(
            row(Some(CliPhase::Working), ago(HOOK_PHASE_TTL_SECS - 1)).fresh_phase(now),
            Some(CliPhase::Working)
        );
        assert_eq!(
            row(Some(CliPhase::Working), ago(HOOK_PHASE_TTL_SECS)).fresh_phase(now),
            None
        );
        // A row the poller alone has ever written never claims a phase.
        assert_eq!(row(None, ago(0)).fresh_phase(now), None);
        assert_eq!(row(Some(CliPhase::Working), None).fresh_phase(now), None);
    }

    #[test]
    fn phase_strings_round_trip_and_bucket() {
        for phase in [
            CliPhase::Ready,
            CliPhase::Working,
            CliPhase::Question,
            CliPhase::Approval,
            CliPhase::Attention,
            CliPhase::Stopped,
            CliPhase::Compacting,
            CliPhase::ToolFailed,
            CliPhase::RateLimit,
            CliPhase::Error,
            CliPhase::Ended,
        ] {
            assert_eq!(CliPhase::parse(phase.as_str()), Some(phase));
        }
        assert_eq!(CliPhase::parse("nope"), None);

        assert_eq!(CliPhase::Working.bucket(), CliActivityState::Running);
        assert_eq!(CliPhase::Compacting.bucket(), CliActivityState::Running);
        assert_eq!(CliPhase::Ready.bucket(), CliActivityState::Idle);
        assert_eq!(CliPhase::Ended.bucket(), CliActivityState::Idle);
        for phase in [
            CliPhase::Question,
            CliPhase::Approval,
            CliPhase::Attention,
            CliPhase::Stopped,
            CliPhase::ToolFailed,
            CliPhase::RateLimit,
            CliPhase::Error,
        ] {
            assert_eq!(phase.bucket(), CliActivityState::Attention);
        }
    }
}
