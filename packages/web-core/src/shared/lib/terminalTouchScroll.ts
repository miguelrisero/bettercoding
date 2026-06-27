import type { Terminal } from '@xterm/xterm';

/**
 * Touch → wheel scroll bridge for xterm.js 5.5.
 *
 * Why this exists: xterm binds its own touch-scroll handlers to `terminal.element`,
 * but they early-return when the application has enabled mouse tracking
 * (`coreMouseService.areMouseEventsActive`). In the CLI pane, tmux/`claude` turn
 * mouse tracking on, so a finger swipe does nothing while the desktop mouse wheel
 * still scrolls. We close the gap by translating a vertical swipe into the SAME
 * synthetic `WheelEvent`s xterm already handles for a real mouse wheel:
 *   - mouse tracking active   → xterm forwards the wheel to the app (tmux/claude scrolls)
 *   - mouse tracking inactive → we stay out of the way so xterm's native touch
 *     handler scrolls local scrollback (no double-scroll).
 *
 * xterm 5.5 coupling — RE-VALIDATE ON UPGRADE (verified in-browser against xterm
 * 5.5; this DOM surface is not covered by the unit tests): the wheel listener is
 * on `terminal.element`; `deltaMode: 1` (DOM_DELTA_LINE) + `deltaY: ±1` bypasses
 * xterm's private pixel→row accumulator and deterministically scrolls exactly one
 * line; the pointer coords must land inside `.xterm-screen` or xterm drops the report.
 *
 * The DOM-free `createTouchScrollController` is the unit-tested seam;
 * `installTerminalTouchScroll` is a thin adapter that wires real touch events to it.
 */

export type Axis = 'undecided' | 'vertical' | 'ignore';

export interface Rect {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

/** Accumulated finger travel (px) per dispatched line-wheel. */
export const WHEEL_STEP_PX = 24;
/** Finger travel (px) before the gesture locks to an axis. */
export const AXIS_LOCK_THRESHOLD_PX = 8;

export function decideAxis(
  dx: number,
  dy: number,
  threshold = AXIS_LOCK_THRESHOLD_PX
): Axis {
  if (Math.abs(dx) < threshold && Math.abs(dy) < threshold) return 'undecided';
  // Ties go to "ignore" so an ambiguous diagonal doesn't hijack horizontal intent.
  return Math.abs(dy) > Math.abs(dx) ? 'vertical' : 'ignore';
}

export function clampToRect(
  x: number,
  y: number,
  rect: Rect
): { x: number; y: number } {
  return {
    x: Math.min(Math.max(x, rect.left + 1), rect.right - 1),
    y: Math.min(Math.max(y, rect.top + 1), rect.bottom - 1),
  };
}

export interface TouchPoint {
  touches: number;
  clientX: number;
  clientY: number;
}

export interface TouchScrollDeps {
  /** Current xterm mouse-tracking mode; the bridge only acts when !== 'none'. */
  getMouseTrackingMode: () => string;
  /** Dispatch one line-wheel: +1 = scroll down (toward newer), -1 = scroll up. */
  dispatchWheel: (direction: 1 | -1, clientX: number, clientY: number) => void;
}

export interface TouchMoveResult {
  /** Whether the caller should `preventDefault()` (we own this gesture). */
  prevent: boolean;
}

/**
 * Pure, DOM-free gesture controller — the testable seam.
 */
export function createTouchScrollController(deps: TouchScrollDeps) {
  let axis: Axis = 'undecided';
  let startX = 0;
  let startY = 0;
  let lastY = 0;
  let accumulated = 0;

  return {
    onTouchStart(p: TouchPoint): void {
      accumulated = 0;
      if (p.touches !== 1) {
        axis = 'ignore'; // pinch / multi-touch — leave it to the browser
        return;
      }
      axis = 'undecided';
      startX = p.clientX;
      startY = p.clientY;
      lastY = p.clientY;
    },

    onTouchMove(p: TouchPoint): TouchMoveResult {
      if (p.touches !== 1) {
        // A second finger landed mid-gesture — abandon scrolling for the rest
        // of this touch sequence so a pinch never bridges to wheel scrolling.
        axis = 'ignore';
        accumulated = 0;
        return { prevent: false };
      }
      if (axis === 'ignore') return { prevent: false };

      if (axis === 'undecided') {
        axis = decideAxis(p.clientX - startX, p.clientY - startY);
        if (axis !== 'vertical') return { prevent: false };
      }

      // Only bridge when xterm's own touch scroll is disabled (mouse tracking on).
      // Otherwise let xterm scroll its scrollback natively — avoids double-scroll.
      if (deps.getMouseTrackingMode() === 'none') return { prevent: false };

      // Natural scrolling: finger up (clientY decreases) reveals newer content
      // (scroll down, +1); finger down reveals history (scroll up, -1).
      accumulated += lastY - p.clientY;
      lastY = p.clientY;

      while (accumulated >= WHEEL_STEP_PX) {
        deps.dispatchWheel(1, p.clientX, p.clientY);
        accumulated -= WHEEL_STEP_PX;
      }
      while (accumulated <= -WHEEL_STEP_PX) {
        deps.dispatchWheel(-1, p.clientX, p.clientY);
        accumulated += WHEEL_STEP_PX;
      }

      // We've committed to the vertical gesture: always prevent page scroll/rubber-band,
      // even on sub-step moves that didn't dispatch a wheel yet.
      return { prevent: true };
    },

    onTouchEnd(remainingTouches = 0): void {
      accumulated = 0;
      // Only re-arm once every finger is up; a partial lift from a pinch must
      // not let the remaining finger resume a bridged scroll from stale state.
      axis = remainingTouches === 0 ? 'undecided' : 'ignore';
    },
  };
}

/**
 * Wire the controller to a live xterm terminal's element. Attach ONCE per created
 * terminal (in the creation branch) and do NOT remove on React unmount — the
 * listeners live on `terminal.element` and tear down with it on
 * `terminal.dispose()`, mirroring the existing `contextmenu`/selection handlers.
 * Returns a disposer for tests and explicit teardown.
 */
export function installTerminalTouchScroll(terminal: Terminal): () => void {
  const el = terminal.element;
  if (!el) return () => {};

  // `.xterm-screen` is created by terminal.open() and is stable for the element's
  // life, so resolve it once. The coords are constant across all wheels drained
  // from a single touchmove, so memoize the clamp and only re-measure when they
  // change — keeps repeated querySelector/getBoundingClientRect off the hot path.
  const screen =
    (el.querySelector('.xterm-screen') as HTMLElement | null) ?? el;
  let lastKey = '';
  let lastPoint = { x: 0, y: 0 };

  const controller = createTouchScrollController({
    getMouseTrackingMode: () => terminal.modes.mouseTrackingMode,
    dispatchWheel: (direction, clientX, clientY) => {
      const key = `${clientX},${clientY}`;
      if (key !== lastKey) {
        lastPoint = clampToRect(
          clientX,
          clientY,
          screen.getBoundingClientRect()
        );
        lastKey = key;
      }
      el.dispatchEvent(
        new WheelEvent('wheel', {
          deltaY: direction,
          deltaMode: 1, // DOM_DELTA_LINE — bypasses xterm's private px→row accumulator
          bubbles: true,
          cancelable: true,
          clientX: lastPoint.x,
          clientY: lastPoint.y,
        })
      );
    },
  });

  const toPoint = (e: TouchEvent): TouchPoint => {
    const t = e.touches[0] ?? e.changedTouches[0];
    return {
      touches: e.touches.length,
      clientX: t?.clientX ?? 0,
      clientY: t?.clientY ?? 0,
    };
  };

  const onStart = (e: TouchEvent) => controller.onTouchStart(toPoint(e));
  const onMove = (e: TouchEvent) => {
    if (controller.onTouchMove(toPoint(e)).prevent && e.cancelable) {
      e.preventDefault();
    }
  };
  const onEnd = (e: TouchEvent) => controller.onTouchEnd(e.touches.length);

  el.addEventListener('touchstart', onStart, { passive: true });
  el.addEventListener('touchmove', onMove, { passive: false });
  el.addEventListener('touchend', onEnd, { passive: true });
  el.addEventListener('touchcancel', onEnd, { passive: true });

  return () => {
    el.removeEventListener('touchstart', onStart);
    el.removeEventListener('touchmove', onMove);
    el.removeEventListener('touchend', onEnd);
    el.removeEventListener('touchcancel', onEnd);
  };
}
