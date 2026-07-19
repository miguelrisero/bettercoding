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

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use db::{
    DBService,
    models::workspace_cli_activity::{CliActivityState, WorkspaceCliActivity},
};
use uuid::Uuid;

use crate::pty::{
    CLI_TMUX_SOCKET, CliClientPresence, PtyService, now_unix_secs,
    refresh_cli_tmux_client_ignore_size, run_cli_tmux, tmux_available, tmux_client_flags_supported,
    workspace_id_from_cli_session_name,
};

/// Poll cadence. Two seconds keeps bucket transitions snappy while the cost
/// stays one `tmux list-panes` fork per tick.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Client sizing is a safety/repair pass, not activity UI state, so keep it to
/// one `list-clients` fork roughly every 30 seconds instead of every poll.
const SIZE_SWEEP_TICKS: u8 = 15;

/// Input newer than a hidden transition can disprove a delayed browser event,
/// but only briefly; old input must never override an explicit hidden record.
const FRESH_INPUT_SECS: u64 = 60;

/// Visible browser heartbeats arrive every minute (see
/// `TerminalProvider.tsx::PRESENCE_HEARTBEAT_MS`). Five missed heartbeats mean
/// the host is gone; visible tabs are not meaningfully timer-throttled.
const VISIBLE_PRESENCE_STALE_SECS: u64 = 5 * 60;

/// Manually attached tmux clients have no browser heartbeat. Keep passive
/// watchers participating until they have been input-idle for twenty minutes.
const MANUAL_CLIENT_IDLE_SECS: u64 = 20 * 60;

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

#[derive(Debug, Clone, Copy)]
struct SizingPresence {
    visible: bool,
    last_visible_ago_secs: u64,
    changed_ago_secs: u64,
}

/// Decide whether one attached tmux client should be excluded from window
/// sizing. Pure over ordinary ages/booleans so precedence and boundary
/// semantics cannot be obscured by tmux or clock I/O.
fn should_ignore_client_size(input_idle_secs: u64, presence: Option<SizingPresence>) -> bool {
    match presence {
        Some(presence) if !presence.visible => {
            // A delayed hidden browser event loses only to demonstrably newer,
            // still-fresh input. Input from before the transition cannot make
            // an explicitly hidden client rejoin sizing.
            !(input_idle_secs < presence.changed_ago_secs && input_idle_secs < FRESH_INPUT_SECS)
        }
        Some(presence) => presence.last_visible_ago_secs >= VISIBLE_PRESENCE_STALE_SECS,
        // Manual clients have no heartbeat, so use input idleness and flag
        // only after the generous passive-watcher threshold.
        None => input_idle_secs > MANUAL_CLIENT_IDLE_SECS,
    }
}

/// `None` gives a never-before-seen client one sweep to acquire its web
/// presence record; a new manual client cannot be stale during that grace.
fn desired_ignore_client_size(
    input_idle_secs: u64,
    presence: Option<SizingPresence>,
    seen_in_previous_sweep: bool,
) -> Option<bool> {
    if presence.is_none() && !seen_in_previous_sweep {
        return None;
    }
    Some(should_ignore_client_size(input_idle_secs, presence))
}

#[derive(Debug, PartialEq, Eq)]
struct TmuxClientRow {
    client_pid: u32,
    client_name: String,
    client_activity: i64,
    ignore_size: bool,
}

/// Parse one tab-delimited `list-clients` row and discard clients outside our
/// strict `vk_<uuid>` session namespace. `client_flags` is comma-separated;
/// `ignore-size` may appear anywhere in it.
fn parse_cli_client_line(line: &str) -> Option<TmuxClientRow> {
    let mut fields = line.split('\t');
    let client_pid = fields.next()?.trim().parse().ok()?;
    let client_name = fields.next()?;
    if client_name.is_empty() {
        return None;
    }
    let client_activity = fields.next()?.trim().parse().ok()?;
    let flags = fields.next()?;
    workspace_id_from_cli_session_name(fields.next()?)?;
    if fields.next().is_some() {
        return None;
    }

    Some(TmuxClientRow {
        client_pid,
        client_name: client_name.to_string(),
        client_activity,
        ignore_size: flags.split(',').any(|flag| flag.trim() == "ignore-size"),
    })
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

#[derive(Default)]
struct ClientSizeSweepState {
    refresh_failures: HashMap<u32, ClientSizeRefreshFailure>,
    seen_client_pids: HashSet<u32>,
}

struct ClientSizeRefreshFailure {
    client_name: String,
    count: u8,
}

struct ClientSizeSweepPermit(Arc<AtomicBool>);

impl Drop for ClientSizeSweepPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl CliActivityMonitor {
    pub fn spawn(db: DBService, pty: PtyService) {
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
            let mut size_sweep_ticks = 0;
            let size_sweep_running = Arc::new(AtomicBool::new(false));
            let size_sweep_state =
                Arc::new(tokio::sync::Mutex::new(ClientSizeSweepState::default()));

            loop {
                interval.tick().await;

                size_sweep_ticks += 1;
                if size_sweep_ticks == SIZE_SWEEP_TICKS {
                    size_sweep_ticks = 0;
                    // Sizing repair must never delay the activity state
                    // machine. Skip this tick instead of queuing overlap.
                    if size_sweep_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        let pty = pty.clone();
                        let running = size_sweep_running.clone();
                        let state = size_sweep_state.clone();
                        tokio::spawn(async move {
                            let _permit = ClientSizeSweepPermit(running);
                            let mut state = state.lock().await;
                            sweep_client_size_flags(&pty, &mut state).await;
                        });
                    }
                }

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

/// Reconcile tmux's per-client flag with web presence. One list command gives
/// every field needed for every workspace. Steady state stops there; a batch
/// with transitions takes one fresh PID/name snapshot before any mutation.
async fn sweep_client_size_flags(pty: &PtyService, state: &mut ClientSizeSweepState) {
    if !tmux_client_flags_supported() {
        return;
    }

    let output = match run_cli_tmux(&[
        "-L",
        CLI_TMUX_SOCKET,
        "list-clients",
        "-F",
        "#{client_pid}\t#{client_name}\t#{client_activity}\t#{client_flags}\t#{session_name}",
    ])
    .await
    {
        Ok(output) => output,
        Err(e) => {
            tracing::debug!("Failed to list tmux clients for size sweep: {e}");
            return;
        }
    };

    // LOAD-BEARING ORDERING — do not harmlessly swap these snapshots. The
    // sweep reads tmux flags above BEFORE snapshotting presence here, while
    // the event path writes the presence registry BEFORE refreshing tmux.
    // A race can therefore only make this sweep re-issue an event decision;
    // it cannot observe the new tmux flag with the old registry state and
    // revert that decision.
    let presence = pty.cli_presence_snapshot();
    let now_instant = std::time::Instant::now();
    let now_unix = now_unix_secs();

    let clients: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_cli_client_line)
        .collect();
    let previous_client_pids = std::mem::replace(
        &mut state.seen_client_pids,
        clients.iter().map(|client| client.client_pid).collect(),
    );
    state.refresh_failures.retain(|client_pid, failure| {
        clients.iter().any(|client| {
            client.client_pid == *client_pid && client.client_name == failure.client_name
        })
    });

    let mut transitions = Vec::new();
    for client in clients {
        let input_idle_secs = now_unix.saturating_sub(client.client_activity).max(0) as u64;
        let sizing_presence = presence
            .get(&client.client_pid)
            .map(|presence| sizing_presence(*presence, now_instant));
        let Some(desired_ignore) = desired_ignore_client_size(
            input_idle_secs,
            sizing_presence,
            previous_client_pids.contains(&client.client_pid),
        ) else {
            continue;
        };
        if desired_ignore == client.ignore_size {
            state.refresh_failures.remove(&client.client_pid);
            continue;
        }
        transitions.push((client, desired_ignore));
    }

    if transitions.is_empty() {
        return;
    }

    let output = match run_cli_tmux(&[
        "-L",
        CLI_TMUX_SOCKET,
        "list-clients",
        "-F",
        "#{client_pid}\t#{client_name}",
    ])
    .await
    {
        Ok(output) => output,
        Err(e) => {
            tracing::debug!("Failed to refresh tmux client map for size transitions: {e}");
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let fresh_clients: HashMap<u32, &str> = stdout
        .lines()
        .filter_map(|line| {
            let (pid, name) = line.split_once('\t')?;
            let pid = pid.trim().parse().ok()?;
            (!name.is_empty()).then_some((pid, name))
        })
        .collect();

    for (client, desired_ignore) in transitions {
        // `client_name` is a /dev/pts/N path and can be recycled after the
        // sweep snapshot. Require the fresh batch snapshot to preserve every
        // planned PID/name mapping so a stale row can never target another
        // terminal.
        match fresh_clients.get(&client.client_pid).copied() {
            Some(fresh_name) if fresh_name == client.client_name => {}
            Some(fresh_name) => {
                state.refresh_failures.remove(&client.client_pid);
                tracing::debug!(
                    client_name = %client.client_name,
                    client_pid = client.client_pid,
                    resolved_name = %fresh_name,
                    "Skipping stale tmux client size transition"
                );
                continue;
            }
            None => {
                state.refresh_failures.remove(&client.client_pid);
                tracing::debug!(
                    client_name = %client.client_name,
                    client_pid = client.client_pid,
                    "Skipping vanished tmux client size transition"
                );
                continue;
            }
        }

        match refresh_cli_tmux_client_ignore_size(&client.client_name, desired_ignore).await {
            Ok(()) => {
                state.refresh_failures.remove(&client.client_pid);
            }
            Err(e) => {
                let failure = state
                    .refresh_failures
                    .entry(client.client_pid)
                    .or_insert_with(|| ClientSizeRefreshFailure {
                        client_name: client.client_name.clone(),
                        count: 0,
                    });
                if failure.client_name != client.client_name {
                    failure.client_name.clone_from(&client.client_name);
                    failure.count = 0;
                }
                failure.count = failure.count.saturating_add(1);
                if failure.count == 2 {
                    tracing::warn!(
                        client_name = %client.client_name,
                        client_pid = client.client_pid,
                        error = %e,
                        "Failed to reconcile tmux client size twice consecutively"
                    );
                    continue;
                }
                // First and later failures stay at debug; the second failure
                // above is the single operator signal until success resets it.
                tracing::debug!(
                    client_name = %client.client_name,
                    client_pid = client.client_pid,
                    error = %e,
                    consecutive_failures = failure.count,
                    "Failed to reconcile tmux client size"
                );
            }
        }
    }
}

fn sizing_presence(presence: CliClientPresence, now: std::time::Instant) -> SizingPresence {
    SizingPresence {
        visible: presence.visible,
        last_visible_ago_secs: now
            .saturating_duration_since(presence.last_visible_at)
            .as_secs(),
        changed_ago_secs: now
            .saturating_duration_since(presence.last_changed_at)
            .as_secs(),
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
    fn hidden_transition_overrides_input_that_came_before_it() {
        let hidden = Some(SizingPresence {
            visible: false,
            last_visible_ago_secs: 10,
            changed_ago_secs: 10,
        });
        assert!(should_ignore_client_size(30, hidden));
    }

    #[test]
    fn input_after_hidden_transition_temporarily_keeps_client_in_sizing() {
        let hidden = Some(SizingPresence {
            visible: false,
            last_visible_ago_secs: 10,
            changed_ago_secs: 10,
        });
        assert!(!should_ignore_client_size(5, hidden));
    }

    #[test]
    fn hidden_input_override_is_strict_at_transition_and_freshness_boundaries() {
        let hidden_ten_seconds_ago = Some(SizingPresence {
            visible: false,
            last_visible_ago_secs: 10,
            changed_ago_secs: 10,
        });
        assert!(should_ignore_client_size(10, hidden_ten_seconds_ago));

        let hidden_before_freshness_window = Some(SizingPresence {
            visible: false,
            last_visible_ago_secs: FRESH_INPUT_SECS + 1,
            changed_ago_secs: FRESH_INPUT_SECS + 1,
        });
        assert!(should_ignore_client_size(
            FRESH_INPUT_SECS,
            hidden_before_freshness_window
        ));
        assert!(!should_ignore_client_size(
            FRESH_INPUT_SECS - 1,
            hidden_before_freshness_window
        ));
    }

    #[test]
    fn fresh_visible_heartbeat_keeps_client_in_sizing() {
        let fresh = Some(SizingPresence {
            visible: true,
            last_visible_ago_secs: VISIBLE_PRESENCE_STALE_SECS - 1,
            changed_ago_secs: VISIBLE_PRESENCE_STALE_SECS - 1,
        });
        assert!(!should_ignore_client_size(FRESH_INPUT_SECS, fresh));
    }

    #[test]
    fn visible_heartbeat_is_stale_at_exact_threshold() {
        let stale = Some(SizingPresence {
            visible: true,
            last_visible_ago_secs: VISIBLE_PRESENCE_STALE_SECS,
            changed_ago_secs: VISIBLE_PRESENCE_STALE_SECS,
        });
        assert!(should_ignore_client_size(FRESH_INPUT_SECS, stale));
    }

    #[test]
    fn manual_client_is_flagged_only_after_twenty_minutes_idle() {
        assert!(!should_ignore_client_size(MANUAL_CLIENT_IDLE_SECS, None));
        assert!(should_ignore_client_size(MANUAL_CLIENT_IDLE_SECS + 1, None));
    }

    #[test]
    fn never_seen_unknown_client_gets_one_sweep_of_grace() {
        let idle = MANUAL_CLIENT_IDLE_SECS + 1;
        assert_eq!(desired_ignore_client_size(idle, None, false), None);
        assert_eq!(desired_ignore_client_size(idle, None, true), Some(true));

        let hidden = Some(SizingPresence {
            visible: false,
            last_visible_ago_secs: 0,
            changed_ago_secs: 0,
        });
        assert_eq!(desired_ignore_client_size(idle, hidden, false), Some(true));
    }

    #[test]
    fn parses_tabbed_cli_client_and_finds_ignore_size_between_flags() {
        let row = parse_cli_client_line(
            "4242\t/dev/pts/30\t999900\tattached,ignore-size,focused\t\
             vk_00000000000000000000000000000001",
        )
        .expect("valid CLI client row");
        assert_eq!(
            row,
            TmuxClientRow {
                client_pid: 4242,
                client_name: "/dev/pts/30".to_string(),
                client_activity: 999900,
                ignore_size: true,
            }
        );

        let unflagged = parse_cli_client_line(
            "4243\t/dev/pts/31\t999901\tattached,focused\t\
             vk_00000000000000000000000000000001",
        )
        .expect("unflagged CLI client row");
        assert!(!unflagged.ignore_size);
    }

    #[test]
    fn client_parser_skips_malformed_and_non_cli_rows() {
        assert!(
            parse_cli_client_line(
                "bad\t/dev/pts/1\t10\tattached\tvk_00000000000000000000000000000001"
            )
            .is_none()
        );
        assert!(
            parse_cli_client_line(
                "1\t/dev/pts/1\tbad\tattached\tvk_00000000000000000000000000000001"
            )
            .is_none()
        );
        assert!(parse_cli_client_line("1\t/dev/pts/1\t10\tattached").is_none());
        assert!(parse_cli_client_line("1\t/dev/pts/1\t10\tattached\twork").is_none());
        assert!(
            parse_cli_client_line(
                "1\t/dev/pts/1\t10\tattached\tvk_00000000000000000000000000000001\textra"
            )
            .is_none()
        );
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
