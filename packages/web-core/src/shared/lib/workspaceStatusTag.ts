import type { CliManual, CliPhase } from 'shared/types';
import type {
  WorkspaceStatusGroup,
  WorkspaceStatusTag,
} from '@vibe/ui/components/WorkspaceSummary';

// Display rules adapted from Herdr's claude activity row (herdr-activity.md):
// the phase the agent reports about itself, whether the user has looked since,
// how old it is, or a state the user set by hand.

const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
/** A live phase with no hook event for this long may be a crashed agent. */
export const SIGNAL_TTL_MS = HOUR_MS;
/** Idle workspaces with no activity for this long are stale. */
export const STALE_MS = 3 * 24 * HOUR_MS;

export interface StatusTagInput {
  cliPhase?: CliPhase | null;
  /** When `cliPhase` was reported. */
  cliPhaseAt?: string | null;
  cliTasks?: number | null;
  cliCrons?: number | null;
  cliManual?: CliManual | null;
  /** Last activity of any kind (execution process or CLI session). */
  activityAt?: string | null;
  // Coarse signals, for agents that report no phase (codex, chat mode).
  isRunning?: boolean;
  hasPendingApproval?: boolean;
  /** Finished while the user was away and not looked at since. */
  hasUnseenActivity?: boolean;
  now?: number;
}

const RANK = {
  approval: 0,
  question: 1,
  error: 2,
  rate_limit: 3,
  attention: 4,
  unread: 5,
  working: 10,
  compacting: 11,
  tool_failed: 12,
  background: 13,
  scheduled: 14,
  no_signal: 15,
  ready: 20,
  checked: 21,
  parked: 22,
  stale: 30,
  closed: 31,
} as const;
type TagKey = keyof typeof RANK;
export const NO_TAG_RANK = 40;

const GROUP_OF = (key: TagKey): WorkspaceStatusGroup => {
  const rank = RANK[key];
  if (rank < 10) return 'needs_you';
  if (rank < 20) return 'running';
  if (rank < 30) return 'idle';
  return 'older';
};

const TONE: Record<WorkspaceStatusGroup, WorkspaceStatusTag['tone']> = {
  needs_you: 'attention',
  running: 'working',
  idle: 'done',
  older: 'muted',
};

const tag = (
  key: TagKey,
  label: string,
  tone?: WorkspaceStatusTag['tone']
): WorkspaceStatusTag => ({
  key,
  label,
  tone: tone ?? TONE[GROUP_OF(key)],
  rank: RANK[key],
  group: GROUP_OF(key),
});

export function formatAge(ms: number): string {
  if (ms < HOUR_MS) return `${Math.max(1, Math.floor(ms / MINUTE_MS))}m`;
  if (ms < 48 * HOUR_MS) return `${Math.floor(ms / HOUR_MS)}h`;
  return `${Math.floor(ms / (24 * HOUR_MS))}d`;
}

const plural = (n: number, one: string, many: string) =>
  `${n} ${n === 1 ? one : many}`;

const parse = (iso?: string | null) => {
  const ms = iso ? Date.parse(iso) : NaN;
  return Number.isNaN(ms) ? null : ms;
};

const LIVE_LABELS = {
  working: '⏳ Working',
  compacting: '↻ Compacting',
  tool_failed: '⚠ Tool failed',
} as const;

/** The workspace's status tag, or `null` when there is nothing to say. */
export function workspaceStatusTag(
  input: StatusTagInput
): WorkspaceStatusTag | null {
  const now = input.now ?? Date.now();
  const { cliPhase: phase, cliManual: manual } = input;
  const phaseAt = parse(input.cliPhaseAt);
  const phaseAge = phaseAt == null ? null : now - phaseAt;
  const lastAt = Math.max(
    phaseAt ?? -Infinity,
    parse(input.activityAt) ?? -Infinity
  );
  const idleAge = Number.isFinite(lastAt) ? now - lastAt : null;
  const isStale = idleAge != null && idleAge >= STALE_MS;
  // A report older than the TTL no longer proves anything about live work.
  const phaseIsOld = phaseAge != null && phaseAge >= SIGNAL_TTL_MS;

  // At rest: the stale rule applies, and a manual state shows.
  const rest = (key: 'ready' | 'checked' | 'parked', label: string) =>
    isStale
      ? tag('stale', `💤 Stale · ${formatAge(idleAge!)}`)
      : tag(key, label);

  if (phase === 'ended') return tag('closed', '⏹ Closed');

  // The pane is producing output but the agent's report is old or absent:
  // something (maybe a hook-less agent) is working there now.
  if (input.isRunning && (!phase || phaseIsOld)) {
    return input.hasPendingApproval
      ? tag('approval', '🔔 Needs approval')
      : tag('working', LIVE_LABELS.working);
  }

  if (manual?.kind === 'locked') {
    return rest(
      'parked',
      manual.note ? `🔒 Parked · ${manual.note}` : '🔒 Parked'
    );
  }

  const tasks = input.cliTasks ?? 0;
  const crons = input.cliCrons ?? 0;
  const atRest =
    phase == null ||
    phase === 'ready' ||
    phase === 'attention' ||
    (phase === 'stopped' && tasks === 0 && crons === 0);
  if (manual?.kind === 'seen' && atRest) return rest('checked', '✓ Checked');

  switch (phase) {
    case 'approval':
      return tag('approval', '🔔 Needs approval');
    case 'question':
      return tag('question', '🔔 Needs answer');
    case 'error':
      return tag('error', '✕ API error');
    case 'rate_limit':
      return tag('rate_limit', '⚠ Rate limited');
    case 'attention':
      return tag('attention', '🔔 Waiting for you');
    case 'ready':
      return rest('ready', '○ Ready');
    case 'working':
    case 'compacting':
    case 'tool_failed':
      if (phaseIsOld) {
        return tag(
          'no_signal',
          `${LIVE_LABELS[phase]} · no signal ${formatAge(phaseAge!)}`,
          'muted'
        );
      }
      return tag(
        phase,
        LIVE_LABELS[phase],
        phase === 'tool_failed' ? 'error' : undefined
      );
    case 'stopped': {
      if (tasks > 0) {
        const parts = [plural(tasks, 'task', 'tasks')];
        if (crons > 0) parts.push(`${crons} scheduled`);
        const label = `⚙ Background · ${parts.join(', ')}`;
        return phaseIsOld
          ? tag(
              'no_signal',
              `${label} · no signal ${formatAge(phaseAge!)}`,
              'muted'
            )
          : tag('background', label);
      }
      // A scheduled wake-up is waiting by design; it has no signal deadline.
      if (crons > 0) return tag('scheduled', `⏰ Scheduled · ${crons}`);
      if (input.hasUnseenActivity) return tag('unread', '🔔 Finished, unread');
      return rest('checked', '✓ Checked');
    }
  }

  // No phase: an agent that reports nothing, or chat mode.
  if (input.hasPendingApproval) return tag('approval', '🔔 Needs approval');
  if (input.hasUnseenActivity) return tag('unread', '🔔 Finished, unread');
  if (idleAge == null) return null;
  return rest('checked', '✓ Checked');
}
