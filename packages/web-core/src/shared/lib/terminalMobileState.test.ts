import { describe, it, expect, vi } from 'vitest';
import type { Terminal } from '@xterm/xterm';

import {
  flashTerminalMobileStatus,
  getTerminalMobileState,
  patchTerminalMobileState,
  subscribeTerminalMobileFlash,
  subscribeTerminalMobileState,
} from './terminalMobileState';

// The store only uses the terminal as a WeakMap key.
const term = () => ({}) as Terminal;

describe('terminal mobile state store', () => {
  it('starts with everything off', () => {
    expect(getTerminalMobileState(term())).toEqual({
      ctrlLatched: false,
      selectMode: false,
      dpadActive: false,
      flingCatch: false,
      scrollOwned: false,
    });
  });

  it('patches change the snapshot identity only on real transitions', () => {
    const t = term();
    const before = getTerminalMobileState(t);
    patchTerminalMobileState(t, { ctrlLatched: false }); // no-op
    expect(getTerminalMobileState(t)).toBe(before);
    patchTerminalMobileState(t, { ctrlLatched: true });
    const after = getTerminalMobileState(t);
    expect(after).not.toBe(before);
    expect(after.ctrlLatched).toBe(true);
  });

  it('notifies subscribers on transitions and not on no-ops', () => {
    const t = term();
    const cb = vi.fn();
    const unsubscribe = subscribeTerminalMobileState(t, cb);
    patchTerminalMobileState(t, { selectMode: false }); // no-op
    expect(cb).not.toHaveBeenCalled();
    patchTerminalMobileState(t, { selectMode: true });
    expect(cb).toHaveBeenCalledTimes(1);
    unsubscribe();
    patchTerminalMobileState(t, { selectMode: false });
    expect(cb).toHaveBeenCalledTimes(1);
  });

  it('isolates state between terminals', () => {
    const a = term();
    const b = term();
    patchTerminalMobileState(a, { dpadActive: true });
    expect(getTerminalMobileState(a).dpadActive).toBe(true);
    expect(getTerminalMobileState(b).dpadActive).toBe(false);
  });
});

describe('flash channel (events, not state)', () => {
  it('delivers messages to live listeners only — nothing is retained', () => {
    const t = term();
    const early = vi.fn();
    flashTerminalMobileStatus(t, 'lost'); // no listener yet — dropped
    const unsubscribe = subscribeTerminalMobileFlash(t, early);
    expect(early).not.toHaveBeenCalled(); // no replay of 'lost'
    flashTerminalMobileStatus(t, 'Pasted');
    expect(early).toHaveBeenCalledWith('Pasted');
    unsubscribe();
    flashTerminalMobileStatus(t, 'after');
    expect(early).toHaveBeenCalledTimes(1);
  });

  it('repeated identical messages each notify', () => {
    const t = term();
    const cb = vi.fn();
    subscribeTerminalMobileFlash(t, cb);
    flashTerminalMobileStatus(t, 'Pasted');
    flashTerminalMobileStatus(t, 'Pasted');
    expect(cb).toHaveBeenCalledTimes(2);
  });
});
