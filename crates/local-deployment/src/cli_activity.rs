//! Background watcher that turns CLI-mode tmux activity into workspace
//! stage state (`workspace_cli_activity` table → sidebar buckets).
//!
//! The chat path gets its Running/Needs-Attention signals from execution
//! processes and agent turns; a claude running inside a CLI tmux session is
//! invisible to all of that. This monitor polls the dedicated tmux socket and
//! derives an equivalent signal from pane behavior:
//!
//! - pane producing output recently            → `running`
//! - run went quiet while nobody was attached  → `attention`
//!   (claude finished while the user was on another workspace)
//! - attached client views the session         → back to `idle`
//! - pane fell back to a plain shell           → `idle` (claude exited)
//!
//! States are written to the DB only on transitions; the SQLite update hook
//! then re-broadcasts the owning workspace's status patch, so the sidebar
//! moves in near-real-time without any new streaming plumbing.

use std::collections::HashMap;

use db::{
    DBService,
    models::workspace_cli_activity::{CliActivityState, WorkspaceCliActivity},
};
use uuid::Uuid;

use crate::pty::{CLI_TMUX_SOCKET, tmux_available, workspace_id_from_cli_session_name};

/// Poll cadence. Two seconds keeps bucket transitions snappy while the cost
/// stays one `tmux list-panes` fork per tick.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Output newer than this counts as "actively running". claude's TUI repaints
/// continuously (spinner/progress) while working, so a working pane never
/// goes this quiet; an idle prompt does within a tick or two.
const ACTIVE_WINDOW_SECS: i64 = 5;

/// Foreground commands that mean the CLI bootstrap fell through to its
/// fallback shell — claude is no longer running in the pane.
const SHELL_COMMANDS: &[&str] = &["zsh", "bash", "sh", "fish", "dash", "ash", "ksh", "tcsh"];

/// One pane-derived observation per workspace CLI session.
#[derive(Debug, Clone, Copy)]
struct Observation {
    /// Any pane in the session runs something other than a plain shell.
    claude_like: bool,
    /// Unix timestamp of the most recent pane output in the session.
    last_activity: i64,
    /// Whether any client (browser terminal) is attached.
    attached: bool,
}

pub struct CliActivityMonitor;

impl CliActivityMonitor {
    pub fn spawn(db: DBService) {
        tokio::spawn(async move {
            if !tmux_available() {
                tracing::debug!("tmux unavailable; CLI activity monitor not started");
                return;
            }

            // Seed from the DB so a server restart doesn't replay transitions
            // that already happened (and so a run that survived the restart
            // inside tmux can still graduate to `attention`).
            let mut states: HashMap<Uuid, CliActivityState> =
                match WorkspaceCliActivity::find_all(&db.pool).await {
                    Ok(rows) => rows
                        .into_iter()
                        .map(|r| (r.workspace_id, r.state))
                        .collect(),
                    Err(e) => {
                        tracing::warn!("Failed to seed CLI activity states: {e}");
                        HashMap::new()
                    }
                };

            let mut interval = tokio::time::interval(POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;

                // A missing tmux server (no sessions yet, or it died) reads
                // as "no sessions": every known session is gone.
                let observations = observe_tmux().await.unwrap_or_default();

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                // Sessions present in tmux: run the state machine.
                for (workspace_id, obs) in &observations {
                    let prev = states
                        .get(workspace_id)
                        .copied()
                        .unwrap_or(CliActivityState::Idle);
                    let next = next_state(prev, *obs, now);
                    if next != prev {
                        Self::record(&db, &mut states, *workspace_id, next).await;
                    }
                }

                // Sessions we knew about that vanished (killed, reboot): their
                // claude is gone, so any lingering state collapses to idle.
                let gone: Vec<Uuid> = states
                    .iter()
                    .filter(|(id, state)| {
                        !observations.contains_key(*id) && **state != CliActivityState::Idle
                    })
                    .map(|(id, _)| *id)
                    .collect();
                for workspace_id in gone {
                    Self::record(&db, &mut states, workspace_id, CliActivityState::Idle).await;
                }
            }
        });
    }

    async fn record(
        db: &DBService,
        states: &mut HashMap<Uuid, CliActivityState>,
        workspace_id: Uuid,
        state: CliActivityState,
    ) {
        // FK failures are expected when the workspace was just deleted while
        // its tmux session lingered; everything else is worth a warning.
        match WorkspaceCliActivity::upsert(&db.pool, workspace_id, state).await {
            Ok(()) => {
                states.insert(workspace_id, state);
            }
            Err(e) => {
                tracing::debug!("Failed to record CLI activity for {workspace_id}: {e}");
            }
        }
    }
}

/// Pure transition function — kept free of I/O so it can be unit tested.
fn next_state(prev: CliActivityState, obs: Observation, now: i64) -> CliActivityState {
    // Bootstrap fell back to a plain shell: claude is not running here.
    if !obs.claude_like {
        return CliActivityState::Idle;
    }

    let active = now - obs.last_activity <= ACTIVE_WINDOW_SECS;
    if active {
        return CliActivityState::Running;
    }

    match prev {
        // A run just went quiet. If the user was watching, it's simply done;
        // if not, raise a hand until they come look.
        CliActivityState::Running => {
            if obs.attached {
                CliActivityState::Idle
            } else {
                CliActivityState::Attention
            }
        }
        // Attention clears the moment the user attaches (opens the pane).
        CliActivityState::Attention => {
            if obs.attached {
                CliActivityState::Idle
            } else {
                CliActivityState::Attention
            }
        }
        CliActivityState::Idle => CliActivityState::Idle,
    }
}

/// Snapshot all `vk_*` sessions on our tmux socket. Returns `None` when the
/// tmux server isn't running (which is indistinguishable from — and treated
/// as — "no sessions").
async fn observe_tmux() -> Option<HashMap<Uuid, Observation>> {
    let output = tokio::process::Command::new("tmux")
        .args([
            "-L",
            CLI_TMUX_SOCKET,
            "list-panes",
            "-a",
            "-F",
            "#{session_name}|#{pane_current_command}|#{window_activity}|#{session_attached}",
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let mut observations: HashMap<Uuid, Observation> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Split from both ends so a '|' inside pane_current_command can't
        // shift the numeric fields.
        let Some((session_name, rest)) = line.split_once('|') else {
            continue;
        };
        let Some((rest, attached)) = rest.rsplit_once('|') else {
            continue;
        };
        let Some((command, activity)) = rest.rsplit_once('|') else {
            continue;
        };
        let Some(workspace_id) = workspace_id_from_cli_session_name(session_name) else {
            continue;
        };

        let last_activity: i64 = activity.trim().parse().unwrap_or(0);
        let attached = attached.trim().parse::<i64>().unwrap_or(0) > 0;
        let claude_like = !SHELL_COMMANDS.contains(&command.trim());

        // A session can have several panes (manual splits); aggregate to the
        // most "alive" view of it.
        observations
            .entry(workspace_id)
            .and_modify(|o| {
                o.claude_like |= claude_like;
                o.last_activity = o.last_activity.max(last_activity);
                o.attached |= attached;
            })
            .or_insert(Observation {
                claude_like,
                last_activity,
                attached,
            });
    }

    Some(observations)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn obs(claude_like: bool, quiet_for: i64, attached: bool) -> Observation {
        Observation {
            claude_like,
            last_activity: NOW - quiet_for,
            attached,
        }
    }

    #[test]
    fn fresh_output_means_running() {
        let next = next_state(CliActivityState::Idle, obs(true, 0, false), NOW);
        assert_eq!(next, CliActivityState::Running);
    }

    #[test]
    fn run_going_quiet_unattended_raises_attention() {
        let next = next_state(CliActivityState::Running, obs(true, 30, false), NOW);
        assert_eq!(next, CliActivityState::Attention);
    }

    #[test]
    fn run_going_quiet_while_watched_is_idle() {
        let next = next_state(CliActivityState::Running, obs(true, 30, true), NOW);
        assert_eq!(next, CliActivityState::Idle);
    }

    #[test]
    fn attention_clears_on_attach_and_persists_otherwise() {
        let held = next_state(CliActivityState::Attention, obs(true, 300, false), NOW);
        assert_eq!(held, CliActivityState::Attention);
        let cleared = next_state(CliActivityState::Attention, obs(true, 300, true), NOW);
        assert_eq!(cleared, CliActivityState::Idle);
    }

    #[test]
    fn shell_fallback_is_always_idle() {
        for prev in [
            CliActivityState::Idle,
            CliActivityState::Running,
            CliActivityState::Attention,
        ] {
            assert_eq!(
                next_state(prev, obs(false, 0, false), NOW),
                CliActivityState::Idle
            );
        }
    }

    #[test]
    fn session_names_round_trip() {
        let id = Uuid::new_v4();
        let name = crate::pty::cli_tmux_session_name(id);
        assert_eq!(workspace_id_from_cli_session_name(&name), Some(id));
        assert_eq!(workspace_id_from_cli_session_name("vk_short"), None);
        assert_eq!(workspace_id_from_cli_session_name("other_session"), None);
    }
}
