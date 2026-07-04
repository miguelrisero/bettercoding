import type { Terminal } from '@xterm/xterm';

import type { ArrowKey } from './terminalKeySequences';
import {
  getTerminalMobileState,
  patchTerminalMobileState,
} from './terminalMobileState';

/**
 * Touch gesture layer for the terminal (Termius-inspired):
 *
 *   - long-press, then drag  → arrow-key D-pad with 3 speed tiers
 *   - double-tap             → Tab (autocomplete)
 *   - three-finger tap       → paste
 *
 * Coexists with the touch→wheel scroll bridge (terminalTouchScroll): a finger
 * that MOVES before the long-press delay is a scroll and this controller
 * stands down; a finger that DWELLS first enters D-pad mode, sets
 * `dpadActive` in the shared mobile state, and the scroll bridge stands down
 * (it checks `isSuppressed`). Select mode disables the whole layer.
 *
 * `createTouchGestureController` is pure and time-injected (every method takes
 * `now`; scheduling is pull-based via `nextTimerAt`) — the unit-tested seam.
 * `installTerminalTouchGestures` binds real events and one timer to it.
 */

export const LONG_PRESS_MS = 350;
export const TAP_SLOP_PX = 12;
export const DOUBLE_TAP_MS = 300;
/** Drag distance (px) from the press origin before arrows start firing. */
export const DPAD_DEAD_ZONE_PX = 14;
/** Max age (ms) of a touch sequence still counting as a three-finger tap. */
export const MULTI_TAP_MS = 500;

/** Speed tiers: drag distance (px) → repeat interval (ms). Ordered. */
export const DPAD_TIERS: ReadonlyArray<{ dist: number; interval: number }> = [
  { dist: 90, interval: 130 },
  { dist: 160, interval: 60 },
  { dist: Infinity, interval: 28 },
];

export function dpadDirection(
  dx: number,
  dy: number,
  deadZone = DPAD_DEAD_ZONE_PX
): ArrowKey | null {
  if (Math.max(Math.abs(dx), Math.abs(dy)) < deadZone) return null;
  if (Math.abs(dx) > Math.abs(dy)) return dx > 0 ? 'right' : 'left';
  return dy > 0 ? 'down' : 'up';
}

export function dpadInterval(distance: number): number {
  for (const tier of DPAD_TIERS) {
    if (distance < tier.dist) return tier.interval;
  }
  return DPAD_TIERS[DPAD_TIERS.length - 1].interval;
}

export interface GesturePoint {
  touches: number;
  clientX: number;
  clientY: number;
}

export interface GestureDeps {
  /** false → the layer is inert for the rest of the touch sequence (select mode). */
  isEnabled: () => boolean;
  sendArrow: (dir: ArrowKey) => void;
  sendTab: () => void;
  paste: () => void;
  /** D-pad engaged/released — drives the overlay + scroll-bridge suppression. */
  setDpad: (active: boolean, dir: ArrowKey | null) => void;
}

export interface GestureMoveResult {
  prevent: boolean;
}

type Phase = 'idle' | 'pressed' | 'dpad' | 'multi' | 'ignored';

export function createTouchGestureController(deps: GestureDeps) {
  let phase: Phase = 'idle';
  let startX = 0;
  let startY = 0;
  let pressAt = 0;
  let maxTouches = 0;
  // D-pad repeat state.
  let dir: ArrowKey | null = null;
  let interval = DPAD_TIERS[0].interval;
  let nextRepeatAt = 0;
  // Double-tap tracking.
  let lastTapAt = 0;
  let lastTapX = 0;
  let lastTapY = 0;
  let hasLastTap = false;

  const exitDpad = () => {
    if (phase === 'dpad') deps.setDpad(false, null);
    dir = null;
  };

  return {
    onTouchStart(p: GesturePoint, now: number): void {
      if (p.touches === 1) {
        if (!deps.isEnabled()) {
          phase = 'ignored';
          return;
        }
        phase = 'pressed';
        startX = p.clientX;
        startY = p.clientY;
        pressAt = now;
        maxTouches = 1;
        return;
      }
      // Additional finger landed. A D-pad in progress is cancelled; otherwise
      // start tracking a potential three-finger tap.
      exitDpad();
      maxTouches = Math.max(maxTouches, p.touches);
      phase = phase === 'ignored' ? 'ignored' : 'multi';
    },

    onTouchMove(p: GesturePoint, now: number): GestureMoveResult {
      if (phase === 'pressed') {
        if (now - pressAt >= LONG_PRESS_MS) {
          // The promotion timer can be starved by a busy main thread (TUI
          // redraw, keyboard resize). The finger DID dwell long enough, so
          // promote here — before the slop check — or the first delayed
          // move would hand a held press to the scroll bridge.
          phase = 'dpad';
          dir = null;
          deps.setDpad(true, null);
          // fall through to D-pad handling of this same move
        } else {
          const moved =
            Math.abs(p.clientX - startX) > TAP_SLOP_PX ||
            Math.abs(p.clientY - startY) > TAP_SLOP_PX;
          if (moved) {
            // Moving before the long-press delay = scroll; the bridge owns it.
            phase = 'ignored';
          }
          return { prevent: false };
        }
      }
      if (phase !== 'dpad') return { prevent: false };

      const dx = p.clientX - startX;
      const dy = p.clientY - startY;
      const nextDir = dpadDirection(dx, dy);
      const distance = Math.max(Math.abs(dx), Math.abs(dy));
      interval = dpadInterval(distance);
      if (nextDir !== dir) {
        dir = nextDir;
        if (dir) {
          // Fire immediately on engage/direction change; repeats follow.
          deps.sendArrow(dir);
          nextRepeatAt = now + interval;
        }
        deps.setDpad(true, dir);
      }
      // The finger is ours while in D-pad mode — never let it scroll the page.
      return { prevent: true };
    },

    onTouchEnd(remainingTouches: number, now: number): void {
      if (remainingTouches > 0) return; // wait for the last finger
      const wasPhase = phase;
      const wasMulti = maxTouches > 1;
      exitDpad();
      phase = 'idle';
      const touchCount = maxTouches;
      maxTouches = 0;

      if (wasPhase === 'ignored' || wasPhase === 'dpad') {
        hasLastTap = false;
        return;
      }
      if (wasMulti) {
        if (touchCount === 3 && now - pressAt <= MULTI_TAP_MS) deps.paste();
        hasLastTap = false;
        return;
      }
      if (wasPhase !== 'pressed' || now - pressAt >= LONG_PRESS_MS) {
        hasLastTap = false;
        return;
      }
      // A clean quick tap. Second one in time + place = Tab.
      if (
        hasLastTap &&
        now - lastTapAt <= DOUBLE_TAP_MS &&
        Math.abs(startX - lastTapX) <= 2 * TAP_SLOP_PX &&
        Math.abs(startY - lastTapY) <= 2 * TAP_SLOP_PX
      ) {
        deps.sendTab();
        hasLastTap = false; // a triple tap is not two Tabs
        return;
      }
      hasLastTap = true;
      lastTapAt = now;
      lastTapX = startX;
      lastTapY = startY;
    },

    /** Run due work: long-press promotion and D-pad key repeats. */
    onTimer(now: number): void {
      if (phase === 'pressed' && now - pressAt >= LONG_PRESS_MS) {
        phase = 'dpad';
        dir = null;
        deps.setDpad(true, null);
        return;
      }
      if (phase === 'dpad' && dir && now >= nextRepeatAt) {
        deps.sendArrow(dir);
        nextRepeatAt = now + interval;
      }
    },

    /** When the adapter should call onTimer next; null = no timer needed. */
    nextTimerAt(): number | null {
      if (phase === 'pressed') return pressAt + LONG_PRESS_MS;
      if (phase === 'dpad' && dir) return nextRepeatAt;
      return null;
    },

    /**
     * Abort whatever is in flight (React detach mid-gesture: touchend may
     * never arrive once the element leaves the DOM). Releases the D-pad —
     * clearing the scroll-bridge suppression — and stops key repeats.
     */
    cancel(): void {
      exitDpad();
      phase = 'idle';
      maxTouches = 0;
      hasLastTap = false;
    },
  };
}

export interface InstallGestureOptions {
  sendArrow: (dir: ArrowKey) => void;
  sendTab: () => void;
  paste: () => void;
}

// Live gesture cancellers, one per terminal. React unmount cleanup calls
// cancelActiveTerminalGesture so a D-pad running at detach time can't keep
// its repeat timer (and the scroll-bridge suppression) alive.
const activeGestureCancels = new WeakMap<Terminal, () => void>();

export function cancelActiveTerminalGesture(terminal: Terminal): void {
  activeGestureCancels.get(terminal)?.();
}

/**
 * Bind the gesture controller to a live terminal element. Like the scroll
 * bridge: attach once per created terminal; listeners live and die with the
 * element, so no unmount cleanup is registered by the caller. Returns a
 * disposer for tests.
 */
export function installTerminalTouchGestures(
  terminal: Terminal,
  options: InstallGestureOptions
): () => void {
  const el = terminal.element;
  if (!el) return () => {};

  // Minimal D-pad overlay: a centered badge showing the active direction.
  const overlay = document.createElement('div');
  overlay.className = 'xterm-dpad-overlay';
  overlay.style.cssText =
    'position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);' +
    'z-index:20;display:none;padding:10px 14px;border-radius:9999px;' +
    'background:rgba(0,0,0,0.55);color:#fff;font-size:20px;line-height:1;' +
    'pointer-events:none;user-select:none;';
  overlay.textContent = '✛';
  el.appendChild(overlay);

  const DIR_GLYPH: Record<ArrowKey, string> = {
    up: '▲',
    down: '▼',
    left: '◀',
    right: '▶',
  };

  let timer: ReturnType<typeof setTimeout> | null = null;

  const controller = createTouchGestureController({
    isEnabled: () => !getTerminalMobileState(terminal).selectMode,
    sendArrow: options.sendArrow,
    sendTab: options.sendTab,
    paste: options.paste,
    setDpad: (active, dirNow) => {
      patchTerminalMobileState(terminal, { dpadActive: active });
      overlay.style.display = active ? 'block' : 'none';
      overlay.textContent = dirNow ? DIR_GLYPH[dirNow] : '✛';
    },
  });

  const reschedule = () => {
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
    const at = controller.nextTimerAt();
    if (at === null) return;
    timer = setTimeout(
      () => {
        controller.onTimer(Date.now());
        reschedule();
      },
      Math.max(0, at - Date.now())
    );
  };

  const toPoint = (e: TouchEvent): GesturePoint => {
    const t = e.touches[0] ?? e.changedTouches[0];
    return {
      touches: e.touches.length,
      clientX: t?.clientX ?? 0,
      clientY: t?.clientY ?? 0,
    };
  };

  const onStart = (e: TouchEvent) => {
    controller.onTouchStart(toPoint(e), Date.now());
    reschedule();
  };
  const onMove = (e: TouchEvent) => {
    if (
      controller.onTouchMove(toPoint(e), Date.now()).prevent &&
      e.cancelable
    ) {
      e.preventDefault();
    }
    reschedule();
  };
  const onEnd = (e: TouchEvent) => {
    controller.onTouchEnd(e.touches.length, Date.now());
    reschedule();
  };

  el.addEventListener('touchstart', onStart, { passive: true });
  el.addEventListener('touchmove', onMove, { passive: false });
  el.addEventListener('touchend', onEnd, { passive: true });
  el.addEventListener('touchcancel', onEnd, { passive: true });

  activeGestureCancels.set(terminal, () => {
    controller.cancel();
    reschedule(); // nextTimerAt() is now null → clears the pending timer
  });

  return () => {
    if (timer) clearTimeout(timer);
    activeGestureCancels.delete(terminal);
    el.removeEventListener('touchstart', onStart);
    el.removeEventListener('touchmove', onMove);
    el.removeEventListener('touchend', onEnd);
    el.removeEventListener('touchcancel', onEnd);
    overlay.remove();
  };
}
