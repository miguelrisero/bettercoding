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
 * that moves beyond slop before the long-press delay is a scroll; any move
 * timestamped at/after the deadline proves a moving finger dwelled. Timer
 * promotion keeps perfectly still presses working, but a queued pre-deadline
 * swipe demotes it before an arrow can fire. A proven dwell enters D-pad mode,
 * sets `dpadActive`, and makes the scroll bridge stand down. Select mode
 * disables the whole layer.
 *
 * `createTouchGestureController` is pure and time-injected; scheduling is
 * pull-based via `nextTimerAt`. Event timestamps classify dwell, slop, demotion,
 * and taps so a busy main thread cannot rewrite what the user did. Repeat
 * cadence and the dispatch rate floor use handler-delivery time instead, so a
 * stale queued event cannot create an already-overdue repeat. The DOM adapter
 * supplies `e.timeStamp` for the former and `performance.now()` for the latter.
 * This split has one load-bearing premise: `TouchEvent.timeStamp` shares
 * `performance.now()`'s time origin, as it does in current browsers.
 * `onTimer`/`nextTimerAt` compare `pressAt + LONG_PRESS_MS` against that
 * delivery clock, so re-validate the premise when supporting a new platform.
 */

export const LONG_PRESS_MS = 350;
export const TAP_SLOP_PX = 12;
export const DOUBLE_TAP_MS = 300;
/**
 * A deliberate drag that starts when a long press feels ready (~320–345ms)
 * can carry a coalesced timestamp just below the deadline; demoting it would
 * silently drop the gesture with no feedback. Genuine fast swipes cross slop
 * within roughly 30–50ms of touchstart, far outside this guard band.
 *
 * The tradeoff is real: someone who holds still for 290–350ms and then flicks
 * to scroll can be read as a D-pad if delivery trails the timer by even ~5–10ms;
 * without the band, that gesture would scroll. Keep the band narrow and do not
 * widen it without weighing that cost.
 */
export const DEMOTE_GUARD_MS = 60;
/** Drag distance (px) from the press origin before arrows start firing. */
export const DPAD_DEAD_ZONE_PX = 14;
/** Distance retained around every D-pad boundary to absorb thumb tremor. */
export const ZONE_HYSTERESIS_PX = 8;
/**
 * Dispatch floor shared by every arrow path; boundary jitter is harmless even
 * if it crosses more quickly than the repeat cadence.
 */
export const MIN_ARROW_INTERVAL_MS = 120;
/** Gap between the press origin and the D-pad feedback badge. */
export const BADGE_OFFSET_PX = 56;
/** Max age (ms) of a touch sequence still counting as a three-finger tap. */
export const MULTI_TAP_MS = 500;

// Owner-specified product behavior ("1 per 0.5 seconds, even more 4 per
// second"): roughly 1/2/4 arrows per second. Felt-speed changes are deliberately
// one-line edits to these constants; nothing else in the state machine encodes
// a rate.
export const DPAD_REPEAT_SLOW_MS = 1_000;
export const DPAD_REPEAT_MEDIUM_MS = 500;
export const DPAD_REPEAT_FAST_MS = 250;

/**
 * D-pad distance/rate tuning. Rows are ordered by inclusive minimum distance.
 * Zone 1: [dead zone, 48px) — one arrow per outward entry, no auto-repeat.
 * Zone 2: [48px, 88px) — slow auto-repeat, about one arrow per second.
 * Zone 3: [88px, 128px) — medium auto-repeat, about two arrows per second.
 * Zone 4: [128px, ∞) — fast auto-repeat, about four arrows per second.
 * Pulling the thresholds inward keeps zone 4 reachable from natural left- and
 * right-thumb origins on a 390px viewport, rather than only near its center.
 */
export const DPAD_ZONES = [
  { minDistance: DPAD_DEAD_ZONE_PX, repeatMs: null },
  { minDistance: 48, repeatMs: DPAD_REPEAT_SLOW_MS },
  { minDistance: 88, repeatMs: DPAD_REPEAT_MEDIUM_MS },
  { minDistance: 128, repeatMs: DPAD_REPEAT_FAST_MS },
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

/**
 * Stateful zone selection with symmetric hysteresis. Moving outward enters a
 * zone at `minDistance + 8`; retreating leaves it below `minDistance - 8`.
 * Zone 0 is the dead zone/no-direction state.
 */
export function dpadZoneWithHysteresis(
  distance: number,
  currentZone: DpadZone | 0
): DpadZone | 0 {
  let nextZone = currentZone;

  while (
    nextZone < DPAD_ZONES.length &&
    distance >=
      DPAD_ZONES[nextZone as 0 | 1 | 2 | 3].minDistance + ZONE_HYSTERESIS_PX
  ) {
    nextZone = (nextZone + 1) as DpadZone;
  }

  while (
    nextZone > 0 &&
    distance < DPAD_ZONES[nextZone - 1].minDistance - ZONE_HYSTERESIS_PX
  ) {
    nextZone = (nextZone - 1) as DpadZone | 0;
  }

  return nextZone;
}

function dpadDirectionWithHysteresis(
  dx: number,
  dy: number,
  currentDirection: ArrowKey
): ArrowKey {
  const candidate = dpadDirection(dx, dy, 0) ?? currentDirection;
  const incumbentIsHorizontal =
    currentDirection === 'left' || currentDirection === 'right';
  const candidateIsHorizontal = candidate === 'left' || candidate === 'right';

  if (incumbentIsHorizontal === candidateIsHorizontal) return candidate;

  const incumbentMagnitude = incumbentIsHorizontal
    ? Math.abs(dx)
    : Math.abs(dy);
  const candidateMagnitude = candidateIsHorizontal
    ? Math.abs(dx)
    : Math.abs(dy);
  if (
    candidateMagnitude >= incumbentMagnitude * 1.3 ||
    candidateMagnitude >= incumbentMagnitude + 10
  ) {
    return candidate;
  }
  return currentDirection;
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
  /** Promotion or any past-slop travel permanently rules out multi-tap paste. */
  let multiTapDisqualified = false;
  /** Any move timestamped at/after the deadline proves this sequence dwelled. */
  let dwellProven = false;
  /** At least one arrow was actually sent during the current touch sequence. */
  let arrowDispatched = false;
  // D-pad repeat state.
  let dir: ArrowKey | null = null;
  let zone: DpadZone | 0 = 0;
  let nextRepeatAt: number | null = null;
  let lastArrowAt: number | null = null;
  // Double-tap tracking.
  let lastTap: { at: number; x: number; y: number } | null = null;

  const dpadOrigin = () => ({ clientX: startX, clientY: startY });

  const scheduleRepeatFrom = (nowForScheduling: number) => {
    const repeatMs = zone === 0 ? null : DPAD_ZONES[zone - 1].repeatMs;
    nextRepeatAt = repeatMs === null ? null : nowForScheduling + repeatMs;
  };

  const dispatchArrow = (
    direction: ArrowKey,
    nowForScheduling: number
  ): boolean => {
    if (
      lastArrowAt !== null &&
      nowForScheduling - lastArrowAt < MIN_ARROW_INTERVAL_MS
    ) {
      return false;
    }
    deps.sendArrow(direction);
    arrowDispatched = true;
    lastArrowAt = nowForScheduling;
    return true;
  };

  const exitDpad = () => {
    if (phase === 'dpad') deps.setDpad(false, null, 1, dpadOrigin());
    dir = null;
    zone = 0;
    nextRepeatAt = null;
  };

  const promoteToDpad = () => {
    phase = 'dpad';
    multiTapDisqualified = true;
    dir = null;
    zone = 0;
    nextRepeatAt = null;
    deps.setDpad(true, null, 1, dpadOrigin());
  };

  return {
    onTouchStart(p: GesturePoint, now: number): void {
      if (p.touches === 1) {
        dwellProven = false;
        arrowDispatched = false;
        lastArrowAt = null;
        multiTapDisqualified = false;
        maxTouches = 1;
        if (!deps.isEnabled()) {
          phase = 'ignored';
          return;
        }
        phase = 'pressed';
        startX = p.clientX;
        startY = p.clientY;
        pressAt = now;
        return;
      }
      if (phase === 'idle') {
        // The whole gesture arrived as one multi-finger touchstart (fingers
        // landing within the same frame — the normal case for a deliberate
        // three-finger tap). Initialize here or `pressAt` would be stale
        // from the previous gesture and the select-mode gate skipped.
        dwellProven = false;
        arrowDispatched = false;
        lastArrowAt = null;
        multiTapDisqualified = false;
        maxTouches = p.touches;
        if (!deps.isEnabled()) {
          phase = 'ignored';
          return;
        }
        phase = 'multi';
        startX = p.clientX;
        startY = p.clientY;
        pressAt = now;
        return;
      }
      // Additional finger landed mid-gesture. A D-pad in progress is
      // cancelled; otherwise keep tracking a potential three-finger tap.
      exitDpad();
      maxTouches = Math.max(maxTouches, p.touches);
      phase = phase === 'ignored' ? 'ignored' : 'multi';
    },

    onTouchMove(
      p: GesturePoint,
      eventAt: number,
      nowForScheduling = eventAt
    ): GestureMoveResult {
      const movedPastSlop =
        Math.abs(p.clientX - startX) > TAP_SLOP_PX ||
        Math.abs(p.clientY - startY) > TAP_SLOP_PX;
      const pressDeadline = pressAt + LONG_PRESS_MS;

      if (phase !== 'idle' && movedPastSlop) {
        multiTapDisqualified = true;
      }
      if (
        (phase === 'pressed' || phase === 'dpad') &&
        !dwellProven &&
        eventAt >= pressDeadline
      ) {
        dwellProven = true;
      }

      if (phase === 'multi') {
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
        } else if (dwellProven) {
          // Event time, rather than handler-delivery time, proves the finger
          // remained within slop through the long-press deadline.
          promoteToDpad();
          // fall through to D-pad handling of this same move
        } else {
          return { prevent: false };
        }
      }
      if (phase !== 'dpad') return { prevent: false };

      if (
        !dwellProven &&
        movedPastSlop &&
        eventAt < pressDeadline - DEMOTE_GUARD_MS
      ) {
        // This move happened well before the deadline but sat queued behind
        // timer promotion. Keep checking until dwell is actually proven: an
        // innocent in-slop event must not consume the demotion protection.
        exitDpad();
        phase = 'ignored';
        return { prevent: false };
      }

      const dx = p.clientX - startX;
      const dy = p.clientY - startY;
      const distance = Math.max(Math.abs(dx), Math.abs(dy));
      const nextZone = dpadZoneWithHysteresis(distance, zone);
      const nextDir =
        nextZone === 0
          ? null
          : dir === null
            ? dpadDirection(dx, dy)
            : dpadDirectionWithHysteresis(dx, dy, dir);
      if (nextDir !== dir) {
        dir = nextDir;
        zone = nextZone;
        if (dir) {
          // Engage and direction changes are always responsive; zone 1 then
          // stays discrete until the finger returns to the dead zone.
          if (dispatchArrow(dir, nowForScheduling)) {
            scheduleRepeatFrom(nowForScheduling);
          }
        } else {
          nextRepeatAt = null;
        }
        deps.setDpad(true, dir, zone || 1, dpadOrigin());
      } else if (dir && nextZone !== zone) {
        const previousZone = zone;
        zone = nextZone as DpadZone;
        const repeatMs = DPAD_ZONES[zone - 1].repeatMs;
        if (zone > previousZone) {
          if (dispatchArrow(dir, nowForScheduling)) {
            scheduleRepeatFrom(nowForScheduling);
          }
        } else if (repeatMs === null) {
          nextRepeatAt = null;
        } else {
          // On retreat, preserve an already-promised sooner tick; the slower
          // cadence takes over after it fires instead of adding an arrow now.
          const slowerDeadline = nowForScheduling + repeatMs;
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
      const wasPhase =
        phase === 'dpad' &&
        !dwellProven &&
        !arrowDispatched &&
        now < pressAt + LONG_PRESS_MS
          ? 'pressed'
          : phase;
      const touchCount = maxTouches;
      const wasMultiTapDisqualified = multiTapDisqualified;
      exitDpad();
      phase = 'idle';
      maxTouches = 0;
      multiTapDisqualified = false;
      dwellProven = false;
      arrowDispatched = false;
      lastArrowAt = null;

      if (wasPhase === 'ignored' || wasPhase === 'dpad') {
        lastTap = null;
        return;
      }
      if (touchCount > 1) {
        if (
          touchCount === 3 &&
          !wasMultiTapDisqualified &&
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
    onTimer(nowForScheduling: number): void {
      if (phase === 'pressed' && nowForScheduling >= pressAt + LONG_PRESS_MS) {
        promoteToDpad();
        return;
      }
      if (
        phase === 'dpad' &&
        dir &&
        nextRepeatAt !== null &&
        nowForScheduling >= nextRepeatAt
      ) {
        dispatchArrow(dir, nowForScheduling);
        scheduleRepeatFrom(nowForScheduling);
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
      multiTapDisqualified = false;
      dwellProven = false;
      arrowDispatched = false;
      lastArrowAt = null;
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
  let primaryTouchIdentifier: number | null = null;
  let documentTouchStartAttached = false;

  const controller = createTouchGestureController({
    isEnabled: () => !getTerminalMobileState(terminal).selectMode,
    sendArrow: options.sendArrow,
    sendTab: options.sendTab,
    paste: options.paste,
    setDpad: (active, dirNow, zoneNow, origin) => {
      patchTerminalMobileState(terminal, { dpadActive: active });
      if (!active) {
        detachDocumentTouchStart();
        overlay.style.display = 'none';
        gestureBounds = null;
        return;
      }
      attachDocumentTouchStart();

      if (gestureBounds === null) {
        // The terminal can resize during a gesture, but keeping the engage-time
        // box makes the badge stable under the thumb and avoids layout reads on
        // every move. A new gesture measures its current box again.
        gestureBounds = el.getBoundingClientRect();
        gestureOrigin = origin;
      }

      overlay.textContent = dpadBadgeText(dirNow, zoneNow);
      overlay.style.display = 'block';

      type BadgeSide = 'above' | 'below' | 'left' | 'right';
      const originLeft = gestureOrigin.clientX - gestureBounds.left;
      const originTop = gestureOrigin.clientY - gestureBounds.top;
      // Sit behind the contact, opposite its drag direction; the spatial
      // offset doubles as direction feedback before the glyph is read.
      const initialSide: BadgeSide =
        dirNow === 'up'
          ? 'below'
          : dirNow === 'left'
            ? 'right'
            : dirNow === 'right'
              ? 'left'
              : 'above';
      const oppositeSide: Record<BadgeSide, BadgeSide> = {
        above: 'below',
        below: 'above',
        left: 'right',
        right: 'left',
      };
      const positionFor = (side: BadgeSide) => {
        switch (side) {
          case 'above':
            return {
              left: originLeft - overlay.offsetWidth / 2,
              top: originTop - BADGE_OFFSET_PX - overlay.offsetHeight,
            };
          case 'below':
            return {
              left: originLeft - overlay.offsetWidth / 2,
              top: originTop + BADGE_OFFSET_PX,
            };
          case 'left':
            return {
              left: originLeft - BADGE_OFFSET_PX - overlay.offsetWidth,
              top: originTop - overlay.offsetHeight / 2,
            };
          case 'right':
            return {
              left: originLeft + BADGE_OFFSET_PX,
              top: originTop - overlay.offsetHeight / 2,
            };
        }
      };
      const fitsInsideTerminal = (position: { left: number; top: number }) =>
        position.left >= 0 &&
        position.top >= 0 &&
        position.left + overlay.offsetWidth <= gestureBounds!.width &&
        position.top + overlay.offsetHeight <= gestureBounds!.height;

      let side = initialSide;
      let position = positionFor(side);
      if (!fitsInsideTerminal(position)) {
        // Standard tooltip flip: try the opposite side before clamping. This
        // preserves separation from the contact instead of pinning the badge
        // underneath the user's finger at a terminal edge.
        side = oppositeSide[side];
        position = positionFor(side);
      }
      const maxLeft = Math.max(0, gestureBounds.width - overlay.offsetWidth);
      const maxTop = Math.max(0, gestureBounds.height - overlay.offsetHeight);
      overlay.style.left = `${Math.min(Math.max(0, position.left), maxLeft)}px`;
      overlay.style.top = `${Math.min(Math.max(0, position.top), maxTop)}px`;
    },
  });

  function attachDocumentTouchStart() {
    if (documentTouchStartAttached) return;
    documentTouchStartAttached = true;
    document.addEventListener('touchstart', onDocumentTouchStart, {
      capture: true,
      passive: true,
    });
  }

  function detachDocumentTouchStart() {
    if (!documentTouchStartAttached) return;
    documentTouchStartAttached = false;
    document.removeEventListener('touchstart', onDocumentTouchStart, true);
  }

  const touchWithIdentifier = (
    touches: TouchList,
    identifier: number
  ): Touch | null => {
    for (let index = 0; index < touches.length; index += 1) {
      const touch = touches[index];
      if (touch.identifier === identifier) return touch;
    }
    return null;
  };

  const toOwnedPoint = (e: TouchEvent): GesturePoint | null => {
    if (primaryTouchIdentifier === null) return null;
    const primary = touchWithIdentifier(
      e.targetTouches,
      primaryTouchIdentifier
    );
    if (!primary) return null;
    return {
      touches: e.targetTouches.length,
      clientX: primary.clientX,
      clientY: primary.clientY,
    };
  };

  const clearTimerHandle = (
    handle: ReturnType<typeof setTimeout> | null
  ): null => {
    if (handle !== null) clearTimeout(handle);
    return null;
  };

  const reschedule = () => {
    const at = controller.nextTimerAt();
    if (at === timerTargetAt) return; // same deadline — keep the timer
    timer = clearTimerHandle(timer);
    timerTargetAt = at;
    if (at === null) return;
    timer = setTimeout(
      () => {
        timer = null;
        timerTargetAt = null;
        controller.onTimer(performance.now());
        reschedule();
      },
      Math.max(0, at - performance.now())
    );
  };

  const releaseTouchOwnership = () => {
    primaryTouchIdentifier = null;
    detachDocumentTouchStart();
  };

  const cancelGesture = () => {
    controller.cancel();
    releaseTouchOwnership();
    reschedule();
  };

  function onDocumentTouchStart(e: TouchEvent) {
    if (e.target === el || (e.target && el!.contains(e.target as Node))) return;
    cancelGesture();
  }

  // e.timeStamp shares performance.now()'s monotonic origin and carries the
  // moment the touch actually happened — not when a busy main thread got
  // around to delivering it.
  const onStart = (e: TouchEvent) => {
    if (primaryTouchIdentifier === null) {
      const initiatingTouch = e.changedTouches[0] ?? e.targetTouches[0];
      if (!initiatingTouch) return;
      primaryTouchIdentifier = initiatingTouch.identifier;
    }
    const point = toOwnedPoint(e);
    if (!point) return;
    controller.onTouchStart(point, e.timeStamp);
    reschedule();
  };
  const onMove = (e: TouchEvent) => {
    const point = toOwnedPoint(e);
    if (!point) return;
    if (
      controller.onTouchMove(point, e.timeStamp, performance.now()).prevent &&
      e.cancelable
    ) {
      e.preventDefault();
    }
    reschedule();
  };
  const onEnd = (e: TouchEvent) => {
    if (primaryTouchIdentifier === null) return;
    const primaryStillOwned = touchWithIdentifier(
      e.targetTouches,
      primaryTouchIdentifier
    );
    if (!primaryStillOwned) {
      // End when OUR initiating contact is gone. Global `touches` may still
      // contain contacts that began on nav/key bars and will never emit an
      // event on this terminal, so waiting for the global last finger strands
      // D-pad suppression and its timer indefinitely.
      controller.onTouchEnd(0, e.timeStamp);
      releaseTouchOwnership();
    } else {
      controller.onTouchEnd(e.targetTouches.length, e.timeStamp);
    }
    reschedule();
  };
  const onCancel = () => {
    // A cancelled sequence (system gesture, palm rejection, browser
    // interruption) must not fire tap/paste actions — and may never deliver
    // another event for the remaining fingers, so don't wait for them.
    cancelGesture();
  };

  el.addEventListener('touchstart', onStart, { passive: true });
  el.addEventListener('touchmove', onMove, { passive: false });
  el.addEventListener('touchend', onEnd, { passive: true });
  el.addEventListener('touchcancel', onCancel, { passive: true });

  activeGestureCancels.set(terminal, onCancel);

  return () => {
    timer = clearTimerHandle(timer);
    timerTargetAt = null;
    controller.cancel();
    releaseTouchOwnership();
    activeGestureCancels.delete(terminal);
    el.removeEventListener('touchstart', onStart);
    el.removeEventListener('touchmove', onMove);
    el.removeEventListener('touchend', onEnd);
    el.removeEventListener('touchcancel', onCancel);
    overlay.remove();
  };
}
