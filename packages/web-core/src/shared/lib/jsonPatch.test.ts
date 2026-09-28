import { produce } from 'immer';
import type { Operation } from 'rfc6902';
import { describe, expect, it } from 'vitest';

import fixture from './__fixtures__/native-feed-patches.json';
import { applyPatchStrict, applyUpsertPatch } from './jsonPatch';

type FeedStep = {
  name: string;
  message: { JsonPatch: Operation[] };
  expected: Record<string, unknown>;
};

const steps = fixture as FeedStep[];

function emptyFeed(): Record<string, unknown> {
  return {
    revision: 0,
    seq: 0,
    entries: [],
    forks: [],
    health: {
      unknown_kinds: 0,
      rescans: 0,
      quarantined_files: 0,
      watch_degraded: false,
      foreign_writer_seen_at: null,
      files: [],
    },
  };
}

/** The same Immer + strict applicator path the native feed stream uses. */
function apply(state: Record<string, unknown>, ops: Operation[]) {
  return produce(state, (draft) => {
    applyPatchStrict(draft, ops);
  });
}

describe('native feed patches (fixture written by the Rust feed socket)', () => {
  it('covers snapshot, append with mid replace, and a revision reset', () => {
    expect(steps.map((step) => step.name)).toEqual([
      'snapshot',
      'append with mid replace',
      'append with fork change',
      'revision reset',
    ]);
  });

  it('reproduces the server document after every message', () => {
    let state = emptyFeed();
    for (const step of steps) {
      state = apply(state, step.message.JsonPatch);
      expect(state, step.name).toEqual(step.expected);
    }
  });

  it('shares untouched entries between updates', () => {
    const snapshot = apply(emptyFeed(), steps[0].message.JsonPatch);
    const next = apply(snapshot, steps[1].message.JsonPatch);
    const before = snapshot.entries as unknown[];
    const after = next.entries as unknown[];
    expect(after[1]).toBe(before[1]);
    expect(after[0]).not.toBe(before[0]);
  });

  it('rejects a delta applied to a drifted copy', () => {
    const snapshot = apply(emptyFeed(), steps[0].message.JsonPatch);
    const drifted = produce(snapshot, (draft) => {
      (draft.entries as unknown[]).pop();
    });
    // The append addresses index 2, past the end of the drifted list.
    expect(() => apply(drifted, steps[1].message.JsonPatch)).toThrow();
    // The lenient applicator would accept it and diverge silently.
    expect(() =>
      produce(drifted, (draft) => {
        applyUpsertPatch(draft, steps[1].message.JsonPatch);
      })
    ).not.toThrow();
  });

  it('leaves the previous state untouched when a patch fails', () => {
    const snapshot = apply(emptyFeed(), steps[0].message.JsonPatch);
    const ops: Operation[] = [
      { op: 'replace', path: '/seq', value: 99 },
      { op: 'add', path: '/entries/9', value: {} },
    ];
    expect(() => apply(snapshot, ops)).toThrow();
    expect(snapshot.seq).toBe(2);
  });
});
