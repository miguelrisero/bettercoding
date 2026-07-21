import type { Terminal } from '@xterm/xterm';

import { getTerminalMobileState } from './terminalMobileState';

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
 * During an owned vertical drag, the bridge samples the rendered row height once
 * and dispatches one line-wheel per row of finger travel for roughly 1:1 content
 * tracking. On release, velocity from the trailing 120ms of touch samples drives
 * an interruptible exponential-decay tail (tau = 325ms) for native-style momentum.
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

/** Finger travel (px) per dispatched line-wheel when row height is unavailable. */
export const FALLBACK_WHEEL_STEP_PX = 16;
/** Finger travel (px) before the gesture locks to an axis. */
export const AXIS_LOCK_THRESHOLD_PX = 8;

const MIN_WHEEL_STEP_PX = 8;
const MAX_WHEEL_STEP_PX = 48;
const VELOCITY_WINDOW_MS = 120;
const VELOCITY_SAMPLE_CAPACITY = 32;
const MIN_VELOCITY_WINDOW_MS = 8;
const MOMENTUM_RELEASE_MAX_AGE_MS = 100;
const MIN_MOMENTUM_START_PX_PER_MS = 0.3;
const MAX_MOMENTUM_PX_PER_MS = 3.5;
const MOMENTUM_STOP_PX_PER_MS = 0.05;
const MOMENTUM_TIME_CONSTANT_MS = 325;
const MAX_MOMENTUM_WHEELS = 150;
const MAX_WHEELS_PER_FRAME = 6;

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
  /** Rendered terminal row height, sampled once at the start of each gesture. */
  getLineHeightPx?: () => number;
  /** Monotonic clock used for release-velocity samples. */
  now?: () => number;
  /** Frame scheduler used by the momentum tail. */
  scheduleFrame?: (cb: (t: number) => void) => number;
  /** Cancels a frame scheduled by `scheduleFrame`. */
  cancelFrame?: (id: number) => void;
  /**
   * Another touch consumer owns the gesture (D-pad after a long-press, select
   * mode). Checked per move and momentum tick; once true during a touch the
   * bridge stands down for the rest of that sequence.
   */
  isSuppressed?: () => boolean;
}

export interface TouchMoveResult {
  /** Whether the caller should `preventDefault()` (we own this gesture). */
  prevent: boolean;
}

interface VelocitySample {
  t: number;
  y: number;
}

function sanitizeWheelStep(stepPx: number | undefined): number {
  if (stepPx === undefined || !Number.isFinite(stepPx) || stepPx <= 0) {
    return FALLBACK_WHEEL_STEP_PX;
  }
  return Math.min(Math.max(stepPx, MIN_WHEEL_STEP_PX), MAX_WHEEL_STEP_PX);
}

/**
 * Pure, DOM-free gesture controller — the testable seam.
 */
export function createTouchScrollController(deps: TouchScrollDeps) {
  const readNow = deps.now ?? (() => 0);
  let axis: Axis = 'undecided';
  let startX = 0;
  let startY = 0;
  let lastY = 0;
  let lastClientX = 0;
  let lastClientY = 0;
  let stepPx = FALLBACK_WHEEL_STEP_PX;
  let accumulated = 0;
  let velocitySamples: VelocitySample[] = [];

  let momentumFrameId: number | undefined;
  let momentumGeneration = 0;
  let momentumVelocity = 0;
  let momentumCarry = 0;
  let momentumWheelCount = 0;
  let momentumPrevT = 0;

  function cancelMomentum(): void {
    momentumGeneration += 1;
    if (momentumFrameId !== undefined) {
      deps.cancelFrame?.(momentumFrameId);
    }
    momentumFrameId = undefined;
    momentumVelocity = 0;
    momentumCarry = 0;
    momentumWheelCount = 0;
    momentumPrevT = 0;
  }

  function scheduleMomentumFrame(): void {
    if (!deps.scheduleFrame) {
      cancelMomentum();
      return;
    }

    const generation = momentumGeneration;
    momentumFrameId = deps.scheduleFrame((nowT) => {
      if (generation !== momentumGeneration) return;
      momentumFrameId = undefined;
      runMomentumFrame(nowT);
    });
  }

  function runMomentumFrame(nowT: number): void {
    if (deps.isSuppressed?.() || deps.getMouseTrackingMode() === 'none') {
      cancelMomentum();
      return;
    }

    const dt = Math.min(Math.max(nowT - momentumPrevT, 1), 64);
    momentumPrevT = nowT;
    momentumVelocity *= Math.exp(-dt / MOMENTUM_TIME_CONSTANT_MS);
    momentumCarry += momentumVelocity * dt;

    let frameWheelCount = 0;
    while (
      momentumCarry >= stepPx &&
      frameWheelCount < MAX_WHEELS_PER_FRAME &&
      momentumWheelCount < MAX_MOMENTUM_WHEELS
    ) {
      deps.dispatchWheel(1, lastClientX, lastClientY);
      momentumCarry -= stepPx;
      frameWheelCount += 1;
      momentumWheelCount += 1;
    }
    while (
      momentumCarry <= -stepPx &&
      frameWheelCount < MAX_WHEELS_PER_FRAME &&
      momentumWheelCount < MAX_MOMENTUM_WHEELS
    ) {
      deps.dispatchWheel(-1, lastClientX, lastClientY);
      momentumCarry += stepPx;
      frameWheelCount += 1;
      momentumWheelCount += 1;
    }

    if (
      Math.abs(momentumVelocity) < MOMENTUM_STOP_PX_PER_MS ||
      momentumWheelCount >= MAX_MOMENTUM_WHEELS
    ) {
      cancelMomentum();
      return;
    }

    scheduleMomentumFrame();
  }

  function appendVelocitySample(t: number, y: number): void {
    velocitySamples.push({ t, y });
    const cutoff = t - VELOCITY_WINDOW_MS;
    while (velocitySamples[0]?.t < cutoff) velocitySamples.shift();
    while (velocitySamples.length > VELOCITY_SAMPLE_CAPACITY) {
      velocitySamples.shift();
    }
  }

  function getExitVelocity(): number | undefined {
    if (velocitySamples.length < 2) return undefined;
    const oldest = velocitySamples[0];
    const newest = velocitySamples[velocitySamples.length - 1];
    if (readNow() - newest.t > MOMENTUM_RELEASE_MAX_AGE_MS) return undefined;
    const dt = newest.t - oldest.t;
    if (dt < MIN_VELOCITY_WINDOW_MS) return undefined;

    const velocity = (oldest.y - newest.y) / dt;
    if (
      !Number.isFinite(velocity) ||
      Math.abs(velocity) < MIN_MOMENTUM_START_PX_PER_MS
    ) {
      return undefined;
    }

    return (
      Math.sign(velocity) * Math.min(Math.abs(velocity), MAX_MOMENTUM_PX_PER_MS)
    );
  }

  function resetGesture(nextAxis: Axis): void {
    axis = nextAxis;
    accumulated = 0;
    velocitySamples = [];
  }

  return {
    onTouchStart(p: TouchPoint): void {
      cancelMomentum();
      resetGesture(p.touches === 1 ? 'undecided' : 'ignore');
      stepPx = sanitizeWheelStep(deps.getLineHeightPx?.());
      startX = p.clientX;
      startY = p.clientY;
      lastY = p.clientY;
      lastClientX = p.clientX;
      lastClientY = p.clientY;

      if (p.touches !== 1) {
        // Pinch / multi-touch — leave it to the browser.
        return;
      }
      appendVelocitySample(readNow(), p.clientY);
    },

    onTouchMove(p: TouchPoint): TouchMoveResult {
      if (p.touches !== 1) {
        // A second finger landed mid-gesture — abandon scrolling for the rest
        // of this touch sequence so a pinch never bridges to wheel scrolling.
        resetGesture('ignore');
        return { prevent: false };
      }
      if (axis === 'ignore') return { prevent: false };

      if (deps.isSuppressed?.()) {
        // The gesture layer (D-pad) or select mode took this touch sequence —
        // never turn its drag into wheel scrolling.
        resetGesture('ignore');
        return { prevent: false };
      }

      if (axis === 'undecided') {
        axis = decideAxis(p.clientX - startX, p.clientY - startY);
        if (axis !== 'vertical') return { prevent: false };
      }

      lastClientX = p.clientX;
      lastClientY = p.clientY;
      appendVelocitySample(readNow(), p.clientY);

      const deltaY = lastY - p.clientY;
      lastY = p.clientY;

      // Only bridge when xterm's own touch scroll is disabled (mouse tracking on).
      // Otherwise let xterm scroll its scrollback natively — avoids double-scroll.
      if (deps.getMouseTrackingMode() === 'none') return { prevent: false };

      // Natural scrolling: finger up (clientY decreases) reveals newer content
      // (scroll down, +1); finger down reveals history (scroll up, -1).
      accumulated += deltaY;

      while (accumulated >= stepPx) {
        deps.dispatchWheel(1, p.clientX, p.clientY);
        accumulated -= stepPx;
      }
      while (accumulated <= -stepPx) {
        deps.dispatchWheel(-1, p.clientX, p.clientY);
        accumulated += stepPx;
      }

      // We've committed to the vertical gesture: always prevent page scroll/rubber-band,
      // even on sub-step moves that didn't dispatch a wheel yet.
      return { prevent: true };
    },

    onTouchEnd(remainingTouches = 0): void {
      cancelMomentum();

      const exitVelocity =
        remainingTouches === 0 &&
        axis === 'vertical' &&
        !deps.isSuppressed?.() &&
        deps.getMouseTrackingMode() !== 'none'
          ? getExitVelocity()
          : undefined;

      // Only re-arm once every finger is up; a partial lift from a pinch must
      // not let the remaining finger resume a bridged scroll from stale state.
      resetGesture(remainingTouches === 0 ? 'undecided' : 'ignore');

      if (exitVelocity !== undefined) {
        momentumVelocity = exitVelocity;
        momentumPrevT = readNow();
        scheduleMomentumFrame();
      }
    },

    onTouchCancel(): void {
      cancelMomentum();
      resetGesture('undecided');
      startX = 0;
      startY = 0;
      lastY = 0;
      lastClientX = 0;
      lastClientY = 0;
      stepPx = FALLBACK_WHEEL_STEP_PX;
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
    getLineHeightPx: () =>
      screen.getBoundingClientRect().height / Math.max(1, terminal.rows),
    now: () => performance.now(),
    scheduleFrame: (cb) => requestAnimationFrame(cb),
    cancelFrame: (id) => cancelAnimationFrame(id),
    isSuppressed: () => {
      const state = getTerminalMobileState(terminal);
      return state.dpadActive || state.selectMode;
    },
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
  const onCancel = () => controller.onTouchCancel();

  el.addEventListener('touchstart', onStart, { passive: true });
  el.addEventListener('touchmove', onMove, { passive: false });
  el.addEventListener('touchend', onEnd, { passive: true });
  el.addEventListener('touchcancel', onCancel, { passive: true });

  return () => {
    controller.onTouchCancel();
    el.removeEventListener('touchstart', onStart);
    el.removeEventListener('touchmove', onMove);
    el.removeEventListener('touchend', onEnd);
    el.removeEventListener('touchcancel', onCancel);
  };
}
