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
  /**
   * One-shot status message from a DOM-level installer (e.g. the paste
   * gesture) for the React controls to flash. The nonce makes every emission
   * a distinct value so repeated identical messages still notify.
   */
  flash: { nonce: number; message: string } | null;
}

let flashNonce = 0;

/** Emit a one-shot status flash for this terminal's mobile controls. */
export function flashTerminalMobileStatus(
  terminal: Terminal,
  message: string
): void {
  patchTerminalMobileState(terminal, {
    flash: { nonce: ++flashNonce, message },
  });
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
      state: {
        ctrlLatched: false,
        selectMode: false,
        dpadActive: false,
        flash: null,
      },
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
  // Drop explicit-undefined keys so a sloppy caller can't blank a field.
  const defined = Object.fromEntries(
    Object.entries(patch).filter(([, value]) => value !== undefined)
  ) as Partial<TerminalMobileState>;
  const changed = (Object.keys(defined) as (keyof TerminalMobileState)[]).some(
    (key) => entry.state[key] !== defined[key]
  );
  if (!changed) return;
  // Snapshot object identity changes only on real transitions so React's
  // useSyncExternalStore consumers don't re-render on no-op patches.
  entry.state = { ...entry.state, ...defined };
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
