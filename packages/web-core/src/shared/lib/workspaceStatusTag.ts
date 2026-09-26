import type { CliManual, CliPhase } from 'shared/types';
import type { WorkspaceStatusTag } from '@vibe/ui/components/WorkspaceSummary';

// Display rules ported from Herdr's claude activity row (herdr-activity.md):
// the phase the agent reports about itself, or a state set by hand.

export interface StatusTagInput {
  cliPhase?: CliPhase | null;
  cliTasks?: number | null;
  cliCrons?: number | null;
  cliManual?: CliManual | null;
  // Coarse signals, for agents that report no phase (codex, gemini, chat mode).
  isRunning?: boolean;
  hasPendingApproval?: boolean;
  hasUnseenActivity?: boolean;
}

// Sort rank, most urgent first. A tag missing from here sorts last.
const RANK: Record<string, number> = {
  approval: 0,
  question: 1,
  error: 2,
  rate_limit: 3,
  attention: 4,
  stopped: 5,
  background: 6,
  tool_failed: 7,
  working: 8,
  compacting: 9,
  ready: 10,
  seen: 11,
  locked: 12,
};
export const NO_TAG_RANK = Object.keys(RANK).length;

const tag = (
  key: string,
  label: string,
  tone: WorkspaceStatusTag['tone']
): WorkspaceStatusTag => ({ key, label, tone, rank: RANK[key] });

const plural = (n: number, one: string, many: string) =>
  `${n} ${n === 1 ? one : many}`;

function stoppedTag(tasks?: number | null, crons?: number | null) {
  if (tasks == null || crons == null) {
    // Any armed work is still worth reporting even if the other list is unknown.
    if (!tasks && !crons) {
      return tag('stopped', '✓ Turn ended · background ?', 'done');
    }
  } else if (tasks === 0 && crons === 0) {
    return tag('stopped', '✓ Turn ended', 'done');
  }
  const parts = [
    tasks ? plural(tasks, 'task', 'tasks') : null,
    crons ? `${crons} scheduled` : null,
  ].filter(Boolean);
  return tag('background', `⚙ Background · ${parts.join(', ')}`, 'working');
}

const PHASE_TAGS: Partial<Record<CliPhase, WorkspaceStatusTag>> = {
  ready: tag('ready', '○ Ready', 'muted'),
  working: tag('working', '⏳ Working', 'working'),
  question: tag('question', '🔔 Needs answer', 'attention'),
  approval: tag('approval', '🔔 Needs approval', 'attention'),
  attention: tag('attention', '🔔 Needs attention', 'attention'),
  compacting: tag('compacting', '↻ Compacting', 'working'),
  tool_failed: tag('tool_failed', '⚠ Tool failed', 'error'),
  rate_limit: tag('rate_limit', '⚠ Rate limited', 'error'),
  error: tag('error', '✕ API error', 'error'),
};

/** The workspace's status tag, or `null` when there is nothing to say. */
export function workspaceStatusTag(
  input: StatusTagInput
): WorkspaceStatusTag | null {
  const { cliPhase: phase, cliManual: manual } = input;

  // An ended session shows nothing, and that beats a lock.
  if (phase === 'ended') return null;

  if (manual?.kind === 'locked') {
    return tag(
      'locked',
      manual.note ? `🔒 Locked · ${manual.note}` : '🔒 Locked',
      'muted'
    );
  }
  // `seen` only shows at rest; while the agent works, the phase wins.
  const atRest = phase
    ? phase === 'stopped' || phase === 'attention' || phase === 'ready'
    : !input.isRunning;
  if (manual?.kind === 'seen' && atRest) {
    return tag('seen', '✓ Seen', 'muted');
  }

  if (phase === 'stopped') return stoppedTag(input.cliTasks, input.cliCrons);
  if (phase) return PHASE_TAGS[phase] ?? null;

  if (input.hasPendingApproval) return PHASE_TAGS.approval!;
  if (input.isRunning) return PHASE_TAGS.working!;
  if (input.hasUnseenActivity) return PHASE_TAGS.attention!;
  return null;
}
