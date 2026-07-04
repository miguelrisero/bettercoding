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
}

interface Entry {
  state: TerminalMobileState;
  listeners: Set<() => void>;
}

const entries = new WeakMap<Terminal, Entry>();

function entryFor(terminal: Terminal): Entry {
  let entry = entries.get(terminal);
  if (!entry) {
    entry = {
      state: { ctrlLatched: false, selectMode: false, dpadActive: false },
      listeners: new Set(),
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
  let changed = false;
  for (const key of Object.keys(patch) as (keyof TerminalMobileState)[]) {
    const value = patch[key];
    if (value !== undefined && entry.state[key] !== value) {
      entry.state[key] = value;
      changed = true;
    }
  }
  if (!changed) return;
  // Snapshot object identity changes only on real transitions so React's
  // useSyncExternalStore consumers don't re-render on no-op patches.
  entry.state = { ...entry.state };
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
