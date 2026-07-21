import { useCallback, useSyncExternalStore } from 'react';
import type { Terminal } from '@xterm/xterm';

/**
 * Per-terminal mutable state shared between the React layer (key bar, mobile
 * controls) and the DOM-level installers (touch scroll, gestures, selection).
 *
 * Why a WeakMap keyed by the Terminal and not React state/refs: terminals
 * outlive their React components (XTermInstance re-mounts reattach a live
 * terminal from the provider registry), while the touch installers and the
 * onData pipe are wired ONCE at terminal creation. Anything created per mount
 * (a ref, a closure) would go stale on reattach; the WeakMap gives both sides
 * the same state object for the terminal's whole life and collects with it.
 */
export interface TerminalMobileState {
  /** Sticky Ctrl is latched; the next single typed character becomes a control code. */
  ctrlLatched: boolean;
  /** Select mode: touch drag selects text; gestures + scroll bridge stand down. */
  selectMode: boolean;
  /** A long-press D-pad gesture is running; the scroll bridge stands down. */
  dpadActive: boolean;
  /** The current touch sequence started by stopping live scroll momentum. */
  flingCatch?: boolean;
}

const EMPTY_STATE: TerminalMobileState = {
  ctrlLatched: false,
  selectMode: false,
  dpadActive: false,
};

interface Entry {
  state: TerminalMobileState;
  listeners: Set<() => void>;
  /** One-shot status listeners (flash pill) — events, not retained state. */
  flashListeners: Set<(message: string) => void>;
}

const entries = new WeakMap<Terminal, Entry>();

function entryFor(terminal: Terminal): Entry {
  let entry = entries.get(terminal);
  if (!entry) {
    entry = {
      state: { ...EMPTY_STATE },
      listeners: new Set(),
      flashListeners: new Set(),
    };
    entries.set(terminal, entry);
  }
  return entry;
}

export function getTerminalMobileState(
  terminal: Terminal
): Readonly<TerminalMobileState> {
  return entryFor(terminal).state;
}

export function patchTerminalMobileState(
  terminal: Terminal,
  patch: Partial<TerminalMobileState>
): void {
  const entry = entryFor(terminal);
  const changed = (Object.keys(patch) as (keyof TerminalMobileState)[]).some(
    (key) => patch[key] !== undefined && entry.state[key] !== patch[key]
  );
  if (!changed) return;
  // Snapshot object identity changes only on real transitions so React's
  // useSyncExternalStore consumers don't re-render on no-op patches.
  entry.state = { ...entry.state, ...patch };
  for (const listener of [...entry.listeners]) listener();
}

export function subscribeTerminalMobileState(
  terminal: Terminal,
  listener: () => void
): () => void {
  const entry = entryFor(terminal);
  entry.listeners.add(listener);
  return () => entry.listeners.delete(listener);
}

/**
 * One-shot status message from a DOM-level installer (e.g. the paste gesture)
 * to whichever mobile controls are currently mounted. Deliberately an EVENT,
 * not a state field: retained flash state would replay a stale "Pasted" pill
 * on every remount against a long-lived terminal.
 */
export function flashTerminalMobileStatus(
  terminal: Terminal,
  message: string
): void {
  for (const listener of [...entryFor(terminal).flashListeners]) {
    listener(message);
  }
}

export function subscribeTerminalMobileFlash(
  terminal: Terminal,
  listener: (message: string) => void
): () => void {
  const entry = entryFor(terminal);
  entry.flashListeners.add(listener);
  return () => entry.flashListeners.delete(listener);
}

/**
 * React view of a terminal's mobile state. Snapshot identity only changes on
 * real transitions (see patchTerminalMobileState), so consumers can safely
 * destructure per-field without extra memoization.
 */
export function useTerminalMobileState(
  terminal: Terminal | null
): Readonly<TerminalMobileState> {
  const subscribe = useCallback(
    (cb: () => void) =>
      terminal ? subscribeTerminalMobileState(terminal, cb) : () => {},
    [terminal]
  );
  return useSyncExternalStore(
    subscribe,
    () => (terminal ? getTerminalMobileState(terminal) : EMPTY_STATE),
    () => EMPTY_STATE
  );
}
