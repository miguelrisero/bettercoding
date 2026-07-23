// @vitest-environment node

import { describe, expect, it } from 'vitest';

import {
  partitionStrip,
  type SubagentDescriptor,
} from './subagent-strip-model';

const MAX_ACTIVE_TABS = 4;
const FINISHED_LINGER_MS = 5_000;
const NOW = 10_000;

function descriptor(
  key: string,
  phase: SubagentDescriptor['phase']
): SubagentDescriptor {
  const status =
    phase === 'active'
      ? ({ status: 'created' } as const)
      : phase === 'done'
        ? ({ status: 'success' } as const)
        : ({ status: 'failed' } as const);

  return {
    key,
    name: key,
    description: `Task ${key}`,
    phase,
    result: null,
    status,
  };
}

function partition(
  descriptors: SubagentDescriptor[],
  doneAtByKey: Record<string, number> = {},
  maxActiveTabs = MAX_ACTIVE_TABS
) {
  return partitionStrip({
    descriptors,
    doneAtByKey,
    now: NOW,
    maxActiveTabs,
    lingerMs: FINISHED_LINGER_MS,
  });
}

function tabKeys(result: ReturnType<typeof partitionStrip>) {
  return result.tabs.map(({ descriptor: item }) => item.key);
}

function drawerKeys(result: ReturnType<typeof partitionStrip>) {
  return result.drawer.map((item) => item.key);
}

describe('partitionStrip', () => {
  it('shows exactly four active tabs without overflow', () => {
    const result = partition([
      descriptor('active-1', 'active'),
      descriptor('active-2', 'active'),
      descriptor('active-3', 'active'),
      descriptor('active-4', 'active'),
    ]);

    expect(tabKeys(result)).toEqual([
      'active-1',
      'active-2',
      'active-3',
      'active-4',
    ]);
    expect(result.tabs.every((tab) => !tab.lingering)).toBe(true);
    expect(result.drawer).toEqual([]);
    expect(result.overflowCount).toBe(0);
    expect(result.activeCount).toBe(4);
    expect(result.doneCount).toBe(0);
  });

  it('folds a fifth active task into more overflow', () => {
    const result = partition([
      descriptor('active-1', 'active'),
      descriptor('active-2', 'active'),
      descriptor('active-3', 'active'),
      descriptor('active-4', 'active'),
      descriptor('active-5', 'active'),
    ]);

    expect(tabKeys(result)).toEqual([
      'active-1',
      'active-2',
      'active-3',
      'active-4',
    ]);
    expect(drawerKeys(result)).toEqual(['active-5']);
    expect(result.overflowCount).toBe(1);
    expect(result.overflowLabelMode).toBe('more');
  });

  it('lingers recent completions and settles them at the exact expiry', () => {
    const result = partition(
      [
        descriptor('recent-done', 'done'),
        descriptor('expired-done', 'done'),
        descriptor('untracked-error', 'error'),
      ],
      {
        'recent-done': NOW - FINISHED_LINGER_MS + 1,
        'expired-done': NOW - FINISHED_LINGER_MS,
      }
    );

    expect(tabKeys(result)).toEqual(['recent-done']);
    expect(result.tabs[0].lingering).toBe(true);
    expect(drawerKeys(result)).toEqual(['expired-done', 'untracked-error']);
    expect(result.doneCount).toBe(3);
    expect(result.overflowLabelMode).toBe('done');
  });

  it('gives active tasks the tab budget before lingering tasks', () => {
    const result = partition(
      [
        descriptor('linger-1', 'done'),
        descriptor('active-1', 'active'),
        descriptor('linger-2', 'error'),
        descriptor('active-2', 'active'),
        descriptor('settled-1', 'done'),
      ],
      {
        'linger-1': NOW - 100,
        'linger-2': NOW - 200,
        'settled-1': NOW - FINISHED_LINGER_MS,
      },
      3
    );

    expect(tabKeys(result)).toEqual(['active-1', 'active-2', 'linger-1']);
    expect(result.tabs.map((tab) => tab.lingering)).toEqual([
      false,
      false,
      true,
    ]);
    expect(drawerKeys(result)).toEqual(['linger-2', 'settled-1']);
    expect(result.activeCount).toBe(2);
    expect(result.doneCount).toBe(3);
    expect(result.overflowLabelMode).toBe('done');
  });

  it('uses the done label when every hidden task is non-active', () => {
    const result = partition(
      [
        descriptor('done-1', 'done'),
        descriptor('error-1', 'error'),
        descriptor('done-2', 'done'),
      ],
      {
        'done-1': NOW - 100,
        'error-1': NOW - 200,
        'done-2': NOW - 300,
      },
      1
    );

    expect(tabKeys(result)).toEqual(['done-1']);
    expect(drawerKeys(result)).toEqual(['error-1', 'done-2']);
    expect(result.overflowLabelMode).toBe('done');
    expect(result.doneCount).toBe(3);
  });

  it('keeps deterministic spawn order within each drawer group', () => {
    const result = partition(
      [
        descriptor('settled-1', 'done'),
        descriptor('active-1', 'active'),
        descriptor('linger-1', 'done'),
        descriptor('active-2', 'active'),
        descriptor('settled-2', 'error'),
        descriptor('linger-2', 'error'),
      ],
      {
        'settled-1': NOW - FINISHED_LINGER_MS,
        'linger-1': NOW - 100,
        'settled-2': NOW - FINISHED_LINGER_MS - 1,
        'linger-2': NOW - 200,
      },
      1
    );

    expect(tabKeys(result)).toEqual(['active-1']);
    expect(drawerKeys(result)).toEqual([
      'active-2',
      'linger-1',
      'linger-2',
      'settled-1',
      'settled-2',
    ]);
    expect(result.overflowLabelMode).toBe('more');
  });
});
