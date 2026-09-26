import { describe, expect, it } from 'vitest';
import { workspaceStatusTag } from './workspaceStatusTag';

const label = (input: Parameters<typeof workspaceStatusTag>[0]) =>
  workspaceStatusTag(input)?.label ?? null;

describe('workspaceStatusTag', () => {
  it('labels a stopped turn from its background counts', () => {
    expect(label({ cliPhase: 'stopped', cliTasks: 0, cliCrons: 0 })).toBe(
      '✓ Turn ended'
    );
    expect(label({ cliPhase: 'stopped', cliTasks: 2, cliCrons: 1 })).toBe(
      '⚙ Background · 2 tasks, 1 scheduled'
    );
    expect(label({ cliPhase: 'stopped', cliTasks: 1, cliCrons: 0 })).toBe(
      '⚙ Background · 1 task'
    );
    expect(label({ cliPhase: 'stopped', cliTasks: null, cliCrons: 0 })).toBe(
      '✓ Turn ended · background ?'
    );
  });

  it('applies the manual precedence', () => {
    const locked = { kind: 'locked' as const, note: 'waiting on c2' };
    const seen = { kind: 'seen' as const, note: null };
    expect(label({ cliPhase: 'working', cliManual: locked })).toBe(
      '🔒 Locked · waiting on c2'
    );
    // Ended beats a lock.
    expect(label({ cliPhase: 'ended', cliManual: locked })).toBeNull();
    // Seen silences a finished turn and background work, but not live work.
    expect(
      label({ cliPhase: 'stopped', cliTasks: 2, cliCrons: 0, cliManual: seen })
    ).toBe('✓ Seen');
    expect(label({ cliPhase: 'working', cliManual: seen })).toBe('⏳ Working');
  });

  it('falls back to coarse signals for agents that report no phase', () => {
    expect(label({ isRunning: true })).toBe('⏳ Working');
    expect(label({ hasPendingApproval: true, isRunning: true })).toBe(
      '🔔 Needs approval'
    );
    expect(label({ hasUnseenActivity: true })).toBe('🔔 Needs attention');
    expect(label({})).toBeNull();
  });

  it('ranks what needs the user above what does not', () => {
    const rank = (input: Parameters<typeof workspaceStatusTag>[0]) =>
      workspaceStatusTag(input)!.rank;
    expect(rank({ cliPhase: 'approval' })).toBeLessThan(
      rank({ cliPhase: 'stopped', cliTasks: 0, cliCrons: 0 })
    );
    expect(
      rank({ cliPhase: 'stopped', cliTasks: 0, cliCrons: 0 })
    ).toBeLessThan(rank({ cliPhase: 'working' }));
    expect(rank({ cliPhase: 'working' })).toBeLessThan(
      rank({ cliManual: { kind: 'locked', note: null } })
    );
  });
});
