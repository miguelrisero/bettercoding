import { describe, expect, it } from 'vitest';
import {
  SIGNAL_TTL_MS,
  STALE_MS,
  workspaceStatusTag,
  type StatusTagInput,
} from './workspaceStatusTag';

const NOW = Date.parse('2026-09-27T12:00:00Z');
const ago = (ms: number) => new Date(NOW - ms).toISOString();
const MIN = 60_000;

const status = (input: StatusTagInput) => {
  const tag = workspaceStatusTag({ now: NOW, cliPhaseAt: ago(MIN), ...input });
  return tag && `${tag.group}: ${tag.label}`;
};

describe('workspaceStatusTag', () => {
  it('puts what blocks on the user in "needs you"', () => {
    expect(status({ cliPhase: 'approval' })).toBe(
      'needs_you: 🔔 Needs approval'
    );
    expect(status({ cliPhase: 'question' })).toBe('needs_you: 🔔 Needs answer');
    expect(status({ cliPhase: 'rate_limit' })).toBe(
      'needs_you: ⚠ Rate limited'
    );
    // Never goes stale while unread.
    expect(
      status({
        cliPhase: 'stopped',
        cliTasks: 0,
        cliCrons: 0,
        hasUnseenActivity: true,
        cliPhaseAt: ago(10 * STALE_MS),
      })
    ).toBe('needs_you: 🔔 Finished, unread');
  });

  it('counts background work and scheduled wake-ups as running', () => {
    expect(status({ cliPhase: 'stopped', cliTasks: 2, cliCrons: 1 })).toBe(
      'running: ⚙ Background · 2 tasks, 1 scheduled'
    );
    expect(status({ cliPhase: 'stopped', cliTasks: 0, cliCrons: 1 })).toBe(
      'running: ⏰ Scheduled · 1'
    );
    // A scheduled wake has no signal deadline.
    expect(
      status({
        cliPhase: 'stopped',
        cliTasks: 0,
        cliCrons: 1,
        cliPhaseAt: ago(5 * SIGNAL_TTL_MS),
      })
    ).toBe('running: ⏰ Scheduled · 1');
  });

  it('flags live phases that went silent past the TTL', () => {
    expect(
      status({ cliPhase: 'working', cliPhaseAt: ago(SIGNAL_TTL_MS) })
    ).toBe('running: ⏳ Working · no signal 1h');
    expect(
      status({
        cliPhase: 'stopped',
        cliTasks: 1,
        cliPhaseAt: ago(3 * SIGNAL_TTL_MS),
      })
    ).toBe('running: ⚙ Background · 1 task · no signal 3h');
    // Output in the pane beats an old report.
    expect(
      status({
        cliPhase: 'working',
        cliPhaseAt: ago(SIGNAL_TTL_MS),
        isRunning: true,
      })
    ).toBe('running: ⏳ Working');
  });

  it('separates checked from stale and closed', () => {
    const done = { cliPhase: 'stopped' as const, cliTasks: 0, cliCrons: 0 };
    expect(status(done)).toBe('idle: ✓ Checked');
    expect(status({ ...done, cliPhaseAt: ago(STALE_MS + MIN) })).toBe(
      'older: 💤 Stale · 3d'
    );
    expect(status({ cliPhase: 'ended' })).toBe('older: ⏹ Closed');
    expect(
      status({ cliPhase: 'ended', cliManual: { kind: 'locked', note: 'x' } })
    ).toBe('older: ⏹ Closed');
  });

  it('applies the manual states', () => {
    const locked = { kind: 'locked' as const, note: 'waiting on c2' };
    const seen = { kind: 'seen' as const, note: null };
    expect(status({ cliPhase: 'working', cliManual: locked })).toBe(
      'idle: 🔒 Parked · waiting on c2'
    );
    expect(
      status({
        cliPhase: 'stopped',
        cliTasks: 0,
        cliCrons: 0,
        cliManual: seen,
        hasUnseenActivity: true,
      })
    ).toBe('idle: ✓ Checked');
    // Seen does not hide live work.
    expect(status({ cliPhase: 'working', cliManual: seen })).toBe(
      'running: ⏳ Working'
    );
  });

  it('falls back to coarse signals for agents that report no phase', () => {
    const coarse = (input: StatusTagInput) =>
      status({ cliPhaseAt: null, ...input });
    expect(coarse({ isRunning: true })).toBe('running: ⏳ Working');
    expect(coarse({ isRunning: true, hasPendingApproval: true })).toBe(
      'needs_you: 🔔 Needs approval'
    );
    expect(coarse({ hasUnseenActivity: true })).toBe(
      'needs_you: 🔔 Finished, unread'
    );
    expect(coarse({ activityAt: ago(MIN) })).toBe('idle: ✓ Checked');
    expect(coarse({ activityAt: ago(STALE_MS) })).toBe('older: 💤 Stale · 3d');
    expect(coarse({})).toBeNull();
  });

  it('ranks needs-you above running above idle above older', () => {
    const rank = (input: StatusTagInput) =>
      workspaceStatusTag({ now: NOW, cliPhaseAt: ago(MIN), ...input })!.rank;
    const ranks = [
      rank({ cliPhase: 'approval' }),
      rank({
        cliPhase: 'stopped',
        cliTasks: 0,
        cliCrons: 0,
        hasUnseenActivity: true,
      }),
      rank({ cliPhase: 'working' }),
      rank({ cliPhase: 'stopped', cliTasks: 1 }),
      rank({ cliPhase: 'stopped', cliTasks: 0, cliCrons: 0 }),
      rank({ cliPhase: 'ended' }),
    ];
    expect([...ranks].sort((a, b) => a - b)).toEqual(ranks);
  });
});
