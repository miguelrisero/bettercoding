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

/// A run only earns `attention` if it produced output at least this long
/// AFTER the user detached. Detaching itself makes claude repaint (the
/// focus-out dims its UI), so the last activity timestamp always lands right
/// at departure — without this grace window every brief visit would raise a
/// hand on the way out (observed live).
const DETACH_SETTLE_SECS: i64 = 5;

/// One pane-derived observation per workspace CLI session.
#[derive(Debug, Clone, Copy)]
struct Observation {
    /// A claude process is alive somewhere in the pane's process tree.
    ///
    /// Detected from /proc, NOT from tmux's `pane_current_command`: the
    /// bootstrap runs claude under a non-interactive `$SHELL -c` wrapper,
    /// which has no job control — claude stays in the wrapper's process
    /// group, so the pane's foreground command reads as the shell forever
    /// even while claude is running (verified live on every prod session).
    claude_like: bool,
    /// Unix timestamp of the most recent pane output in the session.
    last_activity: i64,
    /// Whether any client (browser terminal) is attached.
    attached: bool,
}

/// Snapshot of the process table: pid → (parent pid, "is a claude process").
type ProcSnapshot = HashMap<u32, (u32, bool)>;

/// Scan /proc once per tick. A process counts as claude when its comm is
/// `claude` (native binary) or it's a `node` process whose cmdline mentions
/// claude (npm-shim installs).
fn snapshot_processes() -> ProcSnapshot {
    let mut map = ProcSnapshot::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // /proc/<pid>/stat: `pid (comm) state ppid ...` — comm may contain
        // spaces or parens, so split around the LAST ')'.
        let Some(open) = stat.find('(') else { continue };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let comm = &stat[open + 1..close];
        let mut rest = stat[close + 1..].split_whitespace();
        let _state = rest.next();
        let Some(ppid) = rest.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };

        let is_claude = comm == "claude"
            || (comm == "node"
                && std::fs::read_to_string(entry.path().join("cmdline"))
                    .map(|c| c.contains("claude"))
                    .unwrap_or(false));

        map.insert(pid, (ppid, is_claude));
    }
    map
}

/// Whether `root` or any of its descendants is a claude process. Pure over
/// the snapshot so it can be unit tested; bounded to guard against ppid
/// cycles in a torn /proc scan.
fn subtree_has_claude(root: u32, procs: &ProcSnapshot) -> bool {
    // Child lookup is inverted (we store pid → ppid), so walk the table once
    // collecting the descendant set level by level.
    let mut descendants: Vec<u32> = vec![root];
    let mut seen: std::collections::HashSet<u32> = descendants.iter().copied().collect();
    let mut cursor = 0;
    while cursor < descendants.len() && descendants.len() < 4096 {
        let current = descendants[cursor];
        cursor += 1;
        if let Some((_, is_claude)) = procs.get(&current)
            && *is_claude
        {
            return true;
        }
        for (pid, (ppid, _)) in procs.iter() {
            if *ppid == current && seen.insert(*pid) {
                descendants.push(*pid);
            }
        }
    }
    false
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

            // When each workspace was last observed transitioning to
            // detached (None while attached).
            let mut detached_since: HashMap<Uuid, i64> = HashMap::new();

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
                    if obs.attached {
                        detached_since.remove(workspace_id);
                    } else {
                        detached_since.entry(*workspace_id).or_insert(now);
                    }
                    // Did the pane produce output meaningfully after the user
                    // left? Only such runs graduate to `attention`.
                    let ran_while_detached = detached_since
                        .get(workspace_id)
                        .is_some_and(|since| obs.last_activity >= since + DETACH_SETTLE_SECS);
                    let next = next_state(prev, *obs, ran_while_detached, now);
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
/// `ran_while_detached`: the pane produced output meaningfully after the
/// user detached (see DETACH_SETTLE_SECS).
fn next_state(
    prev: CliActivityState,
    obs: Observation,
    ran_while_detached: bool,
    now: i64,
) -> CliActivityState {
    // Bootstrap fell back to a plain shell: claude is not running here.
    if !obs.claude_like {
        return CliActivityState::Idle;
    }

    let active = now - obs.last_activity <= ACTIVE_WINDOW_SECS;
    if active {
        return CliActivityState::Running;
    }

    match prev {
        // A run just went quiet. Raise a hand only when it kept producing
        // output after the user left — a run that ended while they watched,
        // or whose only post-departure output was the focus-out repaint, is
        // simply done.
        CliActivityState::Running => {
            if !obs.attached && ran_while_detached {
                CliActivityState::Attention
            } else {
                CliActivityState::Idle
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
            "#{session_name}|#{pane_pid}|#{window_activity}|#{session_attached}",
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let procs = snapshot_processes();

    let mut observations: HashMap<Uuid, Observation> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split('|');
        let (Some(session_name), Some(pane_pid), Some(activity), Some(attached)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(workspace_id) = workspace_id_from_cli_session_name(session_name) else {
            continue;
        };

        let last_activity: i64 = activity.trim().parse().unwrap_or(0);
        let attached = attached.trim().parse::<i64>().unwrap_or(0) > 0;
        let claude_like = pane_pid
            .trim()
            .parse::<u32>()
            .map(|pid| subtree_has_claude(pid, &procs))
            .unwrap_or(false);

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

    fn procs(entries: &[(u32, u32, bool)]) -> ProcSnapshot {
        entries
            .iter()
            .map(|(pid, ppid, claude)| (*pid, (*ppid, *claude)))
            .collect()
    }

    #[test]
    fn finds_claude_under_the_bootstrap_shell_wrapper() {
        // The real prod shape: zsh wrapper (pane pid) -> claude child, same
        // process group, so tmux's pane_current_command reads "zsh" forever.
        let snapshot = procs(&[(100, 1, false), (101, 100, true)]);
        assert!(subtree_has_claude(100, &snapshot));
    }

    #[test]
    fn fallback_shell_without_claude_is_not_claude_like() {
        // claude exited; the pane is the exec'd interactive shell, possibly
        // with the user's own commands under it.
        let snapshot = procs(&[(100, 1, false), (102, 100, false)]);
        assert!(!subtree_has_claude(100, &snapshot));
    }

    #[test]
    fn finds_claude_deep_in_the_tree_and_survives_cycles() {
        // zsh -> sh -> node(claude) nesting.
        let snapshot = procs(&[(100, 1, false), (101, 100, false), (102, 101, true)]);
        assert!(subtree_has_claude(100, &snapshot));
        // Torn /proc scans can produce bogus ppid loops; must terminate.
        let cyclic = procs(&[(100, 101, false), (101, 100, false)]);
        assert!(!subtree_has_claude(100, &cyclic));
    }

    #[test]
    fn fresh_output_means_running() {
        let next = next_state(CliActivityState::Idle, obs(true, 0, false), false, NOW);
        assert_eq!(next, CliActivityState::Running);
    }

    #[test]
    fn run_that_kept_going_after_departure_raises_attention() {
        let next = next_state(CliActivityState::Running, obs(true, 30, false), true, NOW);
        assert_eq!(next, CliActivityState::Attention);
    }

    #[test]
    fn run_going_quiet_while_watched_is_idle() {
        let next = next_state(CliActivityState::Running, obs(true, 30, true), false, NOW);
        assert_eq!(next, CliActivityState::Idle);
    }

    #[test]
    fn departure_repaint_blip_does_not_raise_attention() {
        // The only post-detach output was the focus-out repaint (inside the
        // settle window), so ran_while_detached is false.
        let next = next_state(CliActivityState::Running, obs(true, 30, false), false, NOW);
        assert_eq!(next, CliActivityState::Idle);
    }

    #[test]
    fn attention_clears_on_attach_and_persists_otherwise() {
        let held = next_state(
            CliActivityState::Attention,
            obs(true, 300, false),
            true,
            NOW,
        );
        assert_eq!(held, CliActivityState::Attention);
        let cleared = next_state(
            CliActivityState::Attention,
            obs(true, 300, true),
            false,
            NOW,
        );
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
                next_state(prev, obs(false, 0, false), false, NOW),
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
