import type { Terminal } from '@xterm/xterm';

import type { ArrowKey } from './terminalKeySequences';
import {
  getTerminalMobileState,
  patchTerminalMobileState,
} from './terminalMobileState';

/**
 * Touch gesture layer for the terminal (Termius-inspired):
 *
 *   - long-press, then drag  → arrow-key D-pad with 4 distance zones
 *   - double-tap             → Tab (autocomplete)
 *   - three-finger tap       → paste
 *
 * Max-axis drag distance selects the D-pad zone. Zone 1 is discrete: pumping
 * dead zone→zone 1 sends one arrow per outward entry. Zones 2–4 repeat at
 * 1/2/4 arrows per second, and the badge repeats an ASCII direction by zone.
 *
 * Coexists with the touch→wheel scroll bridge (terminalTouchScroll): a finger
 * that moves beyond slop before the long-press delay is a scroll; only an
 * in-slop event at/after the deadline proves a moving finger dwelled. Timer
 * promotion keeps perfectly still presses working, but a queued pre-deadline
 * swipe demotes it before an arrow can fire. A proven dwell enters D-pad mode,
 * sets `dpadActive`, and makes the scroll bridge stand down. Select mode
 * disables the whole layer.
 *
 * `createTouchGestureController` is pure and time-injected (every method takes
 * `now`; scheduling is pull-based via `nextTimerAt`) — the unit-tested seam.
 * `installTerminalTouchGestures` binds real events and one timer to it, using
 * event timestamps (not handler-delivery time) so a busy main thread can't
 * misclassify a fast swipe as a dwell, and the monotonic clock so NTP steps
 * can't stall repeats mid-gesture.
 */

export const LONG_PRESS_MS = 350;
export const TAP_SLOP_PX = 12;
export const DOUBLE_TAP_MS = 300;
/** Drag distance (px) from the press origin before arrows start firing. */
export const DPAD_DEAD_ZONE_PX = 14;
/** Max age (ms) of a touch sequence still counting as a three-finger tap. */
export const MULTI_TAP_MS = 500;

/**
 * D-pad distance/rate tuning. Rows are ordered by inclusive minimum distance.
 * Zone 1: [dead zone, 64px) — one arrow per outward entry, no auto-repeat.
 * Zone 2: [64px, 114px) — slow auto-repeat, about one arrow per second.
 * Zone 3: [114px, 164px) — medium auto-repeat, about two arrows per second.
 * Zone 4: [164px, ∞) — fast auto-repeat, about four arrows per second.
 */
export const DPAD_ZONES = [
  { minDistance: DPAD_DEAD_ZONE_PX, repeatMs: null },
  { minDistance: 64, repeatMs: 1_000 },
  { minDistance: 114, repeatMs: 500 },
  { minDistance: 164, repeatMs: 250 },
] as const;

export type DpadZone = 1 | 2 | 3 | 4;

export function dpadDirection(
  dx: number,
  dy: number,
  deadZone = DPAD_DEAD_ZONE_PX
): ArrowKey | null {
  if (Math.max(Math.abs(dx), Math.abs(dy)) < deadZone) return null;
  if (Math.abs(dx) > Math.abs(dy)) return dx > 0 ? 'right' : 'left';
  return dy > 0 ? 'down' : 'up';
}

/**
 * Map distance to its half-open D-pad zone. Below the dead zone this returns
 * zone 1, but callers suppress it because `dpadDirection` is null there.
 */
export function dpadZone(distance: number): DpadZone {
  for (let index = DPAD_ZONES.length - 1; index >= 0; index -= 1) {
    if (distance >= DPAD_ZONES[index].minDistance) {
      return (index + 1) as DpadZone;
    }
  }
  return 1;
}

const DPAD_BADGE_GLYPH: Record<ArrowKey, string> = {
  up: '^',
  down: 'v',
  left: '<',
  right: '>',
};

/** ASCII-only because typographic arrow glyphs can render as tofu on Android. */
export function dpadBadgeText(dir: ArrowKey | null, zone: number): string {
  return dir ? DPAD_BADGE_GLYPH[dir].repeat(zone) : '+';
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
  setDpad: (
    active: boolean,
    dir: ArrowKey | null,
    zone: DpadZone,
    origin: Pick<GesturePoint, 'clientX' | 'clientY'>
  ) => void;
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
  /** A multi-touch gesture travelled beyond slop — it's a swipe, not a tap. */
  let multiMoved = false;
  /** An in-slop move at/after the deadline proves this sequence really dwelled. */
  let dwellProven = false;
  /** The first move after timer promotion may expose a queued earlier swipe. */
  let timerPromotionAwaitingMove = false;
  // D-pad repeat state.
  let dir: ArrowKey | null = null;
  let zone: DpadZone = 1;
  let nextRepeatAt: number | null = null;
  // Double-tap tracking.
  let lastTap: { at: number; x: number; y: number } | null = null;

  const dpadOrigin = () => ({ clientX: startX, clientY: startY });

  const scheduleRepeatFrom = (now: number) => {
    const repeatMs = DPAD_ZONES[zone - 1].repeatMs;
    nextRepeatAt = repeatMs === null ? null : now + repeatMs;
  };

  const exitDpad = () => {
    if (phase === 'dpad') deps.setDpad(false, null, 1, dpadOrigin());
    dir = null;
    zone = 1;
    nextRepeatAt = null;
    timerPromotionAwaitingMove = false;
  };

  const promoteToDpad = (fromTimer = false) => {
    phase = 'dpad';
    dir = null;
    zone = 1;
    nextRepeatAt = null;
    timerPromotionAwaitingMove = fromTimer;
    deps.setDpad(true, null, zone, dpadOrigin());
  };

  return {
    onTouchStart(p: GesturePoint, now: number): void {
      dwellProven = false;
      timerPromotionAwaitingMove = false;
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
        multiMoved = false;
        return;
      }
      if (phase === 'idle') {
        // The whole gesture arrived as one multi-finger touchstart (fingers
        // landing within the same frame — the normal case for a deliberate
        // three-finger tap). Initialize here or `pressAt` would be stale
        // from the previous gesture and the select-mode gate skipped.
        if (!deps.isEnabled()) {
          phase = 'ignored';
          return;
        }
        phase = 'multi';
        startX = p.clientX;
        startY = p.clientY;
        pressAt = now;
        maxTouches = p.touches;
        multiMoved = false;
        return;
      }
      // Additional finger landed mid-gesture. A D-pad in progress is
      // cancelled; otherwise keep tracking a potential three-finger tap.
      exitDpad();
      maxTouches = Math.max(maxTouches, p.touches);
      phase = phase === 'ignored' ? 'ignored' : 'multi';
    },

    onTouchMove(p: GesturePoint, now: number): GestureMoveResult {
      const movedPastSlop =
        Math.abs(p.clientX - startX) > TAP_SLOP_PX ||
        Math.abs(p.clientY - startY) > TAP_SLOP_PX;

      if (phase === 'multi') {
        // A travelling multi-touch is a swipe/pinch (e.g. iPadOS three-finger
        // system gestures) — it must never count as a three-finger TAP.
        if (movedPastSlop) multiMoved = true;
        return { prevent: false };
      }

      if (phase === 'pressed') {
        if (movedPastSlop) {
          if (!dwellProven) {
            // Ambiguous late delivery belongs to scrolling: injecting an
            // arrow is worse than asking for another long press.
            phase = 'ignored';
            return { prevent: false };
          }
          promoteToDpad();
          // fall through to D-pad handling of this same move
        } else if (now - pressAt >= LONG_PRESS_MS) {
          // Event time, rather than handler-delivery time, proves the finger
          // remained within slop through the long-press deadline.
          dwellProven = true;
          promoteToDpad();
          // fall through to D-pad handling of this same move
        } else {
          return { prevent: false };
        }
      }
      if (phase !== 'dpad') return { prevent: false };

      if (timerPromotionAwaitingMove) {
        timerPromotionAwaitingMove = false;
        if (now < pressAt + LONG_PRESS_MS && movedPastSlop) {
          // This move happened before the deadline but sat queued behind the
          // timer. Demote synchronously: our listener runs before the scroll
          // bridge, so this same event reaches it with suppression cleared.
          exitDpad();
          phase = 'ignored';
          return { prevent: false };
        }
      }

      const dx = p.clientX - startX;
      const dy = p.clientY - startY;
      const nextDir = dpadDirection(dx, dy);
      const distance = Math.max(Math.abs(dx), Math.abs(dy));
      const nextZone = dpadZone(distance);
      if (nextDir !== dir) {
        dir = nextDir;
        zone = nextZone;
        nextRepeatAt = null;
        if (dir) {
          // Engage and direction changes are always responsive; zone 1 then
          // stays discrete until the finger returns to the dead zone.
          deps.sendArrow(dir);
          scheduleRepeatFrom(now);
        }
        deps.setDpad(true, dir, zone, dpadOrigin());
      } else if (dir && nextZone !== zone) {
        const previousZone = zone;
        zone = nextZone;
        const repeatMs = DPAD_ZONES[zone - 1].repeatMs;
        if (zone > previousZone) {
          deps.sendArrow(dir);
          scheduleRepeatFrom(now);
        } else if (repeatMs === null) {
          nextRepeatAt = null;
        } else {
          // On retreat, preserve an already-promised sooner tick; the slower
          // cadence takes over after it fires instead of adding an arrow now.
          const slowerDeadline = now + repeatMs;
          nextRepeatAt =
            nextRepeatAt === null
              ? slowerDeadline
              : Math.min(nextRepeatAt, slowerDeadline);
        }
        deps.setDpad(true, dir, zone, dpadOrigin());
      }
      // The finger is ours while in D-pad mode — never let it scroll the page.
      return { prevent: true };
    },

    onTouchEnd(remainingTouches: number, now: number): void {
      if (remainingTouches > 0) return; // wait for the last finger
      const wasPhase = phase;
      const touchCount = maxTouches;
      const wasMultiMoved = multiMoved;
      exitDpad();
      phase = 'idle';
      maxTouches = 0;
      multiMoved = false;
      dwellProven = false;
      timerPromotionAwaitingMove = false;

      if (wasPhase === 'ignored' || wasPhase === 'dpad') {
        lastTap = null;
        return;
      }
      if (touchCount > 1) {
        if (
          touchCount === 3 &&
          !wasMultiMoved &&
          now - pressAt <= MULTI_TAP_MS
        ) {
          deps.paste();
        }
        lastTap = null;
        return;
      }
      if (wasPhase !== 'pressed' || now - pressAt >= LONG_PRESS_MS) {
        lastTap = null;
        return;
      }
      // A clean quick tap. Second one in time + place = Tab.
      if (
        lastTap &&
        now - lastTap.at <= DOUBLE_TAP_MS &&
        Math.abs(startX - lastTap.x) <= 2 * TAP_SLOP_PX &&
        Math.abs(startY - lastTap.y) <= 2 * TAP_SLOP_PX
      ) {
        deps.sendTab();
        lastTap = null; // a triple tap is not two Tabs
        return;
      }
      lastTap = { at: now, x: startX, y: startY };
    },

    /** Run due work: long-press promotion and D-pad key repeats. */
    onTimer(now: number): void {
      if (phase === 'pressed' && now - pressAt >= LONG_PRESS_MS) {
        promoteToDpad(true);
        return;
      }
      if (
        phase === 'dpad' &&
        dir &&
        nextRepeatAt !== null &&
        now >= nextRepeatAt
      ) {
        deps.sendArrow(dir);
        scheduleRepeatFrom(now);
      }
    },

    /** When the adapter should call onTimer next; null = no timer needed. */
    nextTimerAt(): number | null {
      if (phase === 'pressed') return pressAt + LONG_PRESS_MS;
      if (phase === 'dpad' && dir) return nextRepeatAt;
      return null;
    },

    /**
     * Abort whatever is in flight (touchcancel, or React detach mid-gesture:
     * touchend may never arrive once the element leaves the DOM). Releases
     * the D-pad — clearing the scroll-bridge suppression — and stops key
     * repeats. Also the touchcancel path: a cancelled sequence must never
     * count as a tap or three-finger paste.
     */
    cancel(): void {
      exitDpad();
      phase = 'idle';
      maxTouches = 0;
      multiMoved = false;
      dwellProven = false;
      timerPromotionAwaitingMove = false;
      lastTap = null;
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

  // One unobtrusive badge follows the active gesture near its press origin.
  const overlay = document.createElement('div');
  overlay.className = 'xterm-dpad-overlay';
  overlay.style.cssText =
    'position:absolute;top:0;left:0;z-index:20;display:none;' +
    'box-sizing:border-box;max-width:100%;max-height:100%;padding:6px 10px;' +
    'border-radius:9999px;background:rgba(0,0,0,0.72);color:#fff;' +
    'box-shadow:0 1px 3px rgba(0,0,0,0.35);font-size:16px;' +
    'font-family:ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace;' +
    'line-height:1;white-space:nowrap;overflow:hidden;pointer-events:none;' +
    'user-select:none;';
  overlay.textContent = '+';
  el.appendChild(overlay);

  let timer: ReturnType<typeof setTimeout> | null = null;
  let timerTargetAt: number | null = null;
  let gestureBounds: DOMRect | null = null;
  let gestureOrigin = { clientX: 0, clientY: 0 };

  const controller = createTouchGestureController({
    isEnabled: () => !getTerminalMobileState(terminal).selectMode,
    sendArrow: options.sendArrow,
    sendTab: options.sendTab,
    paste: options.paste,
    setDpad: (active, dirNow, zoneNow, origin) => {
      patchTerminalMobileState(terminal, { dpadActive: active });
      if (!active) {
        overlay.style.display = 'none';
        gestureBounds = null;
        return;
      }

      if (gestureBounds === null) {
        // The terminal can resize during a gesture, but keeping the engage-time
        // box makes the badge stable under the thumb and avoids layout reads on
        // every move. A new gesture measures its current box again.
        gestureBounds = el.getBoundingClientRect();
        gestureOrigin = origin;
      }

      overlay.textContent = dpadBadgeText(dirNow, zoneNow);
      overlay.style.display = 'block';

      const desiredLeft =
        gestureOrigin.clientX - gestureBounds.left - overlay.offsetWidth / 2;
      const desiredTop =
        gestureOrigin.clientY - gestureBounds.top - 56 - overlay.offsetHeight;
      const maxLeft = Math.max(0, gestureBounds.width - overlay.offsetWidth);
      const maxTop = Math.max(0, gestureBounds.height - overlay.offsetHeight);
      overlay.style.left = `${Math.min(Math.max(0, desiredLeft), maxLeft)}px`;
      overlay.style.top = `${Math.min(Math.max(0, desiredTop), maxTop)}px`;
    },
  });

  const reschedule = () => {
    const at = controller.nextTimerAt();
    if (at === timerTargetAt) return; // same deadline — keep the timer
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
    timerTargetAt = at;
    if (at === null) return;
    timer = setTimeout(
      () => {
        timerTargetAt = null;
        controller.onTimer(performance.now());
        reschedule();
      },
      Math.max(0, at - performance.now())
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

  // e.timeStamp shares performance.now()'s monotonic origin and carries the
  // moment the touch actually happened — not when a busy main thread got
  // around to delivering it.
  const onStart = (e: TouchEvent) => {
    controller.onTouchStart(toPoint(e), e.timeStamp);
    reschedule();
  };
  const onMove = (e: TouchEvent) => {
    if (
      controller.onTouchMove(toPoint(e), e.timeStamp).prevent &&
      e.cancelable
    ) {
      e.preventDefault();
    }
    reschedule();
  };
  const onEnd = (e: TouchEvent) => {
    controller.onTouchEnd(e.touches.length, e.timeStamp);
    reschedule();
  };
  const onCancel = () => {
    // A cancelled sequence (system gesture, palm rejection, browser
    // interruption) must not fire tap/paste actions — and may never deliver
    // another event for the remaining fingers, so don't wait for them.
    controller.cancel();
    reschedule();
  };

  el.addEventListener('touchstart', onStart, { passive: true });
  el.addEventListener('touchmove', onMove, { passive: false });
  el.addEventListener('touchend', onEnd, { passive: true });
  el.addEventListener('touchcancel', onCancel, { passive: true });

  activeGestureCancels.set(terminal, onCancel);

  return () => {
    if (timer) clearTimeout(timer);
    activeGestureCancels.delete(terminal);
    el.removeEventListener('touchstart', onStart);
    el.removeEventListener('touchmove', onMove);
    el.removeEventListener('touchend', onEnd);
    el.removeEventListener('touchcancel', onCancel);
    overlay.remove();
  };
}
