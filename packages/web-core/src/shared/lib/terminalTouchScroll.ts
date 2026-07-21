import type { Terminal } from '@xterm/xterm';

import {
  getTerminalMobileState,
  patchTerminalMobileState,
} from './terminalMobileState';

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
 * line; the pointer coords must land inside `.xterm-screen` or xterm drops the
 * report. The adapter's `el.isConnected` gate is part of this untested DOM
 * surface too: it must stay ahead of every `terminal.modes` read so detach or
 * dispose cancels momentum without touching a disposed terminal API.
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
export const MOMENTUM_MAX_FRAME_GAP_MS = 250;
const VELOCITY_DIRECTION_EPSILON_PX = 1;

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
  /** Event occurrence time; falls back to handler-delivery time when omitted. */
  timeStampMs?: number;
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

export interface TouchStartResult {
  /** Whether this touchstart stopped a live momentum tail. */
  flingCatch: boolean;
}

export interface TouchEndResult {
  /** A live tail was caught and this sequence remained a tap. */
  caughtFling: boolean;
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
  let lastClientX = 0;
  let lastClientY = 0;
  let stepPx = FALLBACK_WHEEL_STEP_PX;
  let accumulated = 0;
  let velocitySamples: VelocitySample[] = [];
  let sequenceBeganAsCatch = false;
  let verticalTravelOccurred = false;

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

    const elapsed = nowT - momentumPrevT;
    if (!Number.isFinite(elapsed) || elapsed > MOMENTUM_MAX_FRAME_GAP_MS) {
      cancelMomentum();
      return;
    }
    const dt = Math.max(elapsed, 1);
    momentumPrevT = nowT;
    momentumVelocity *= Math.exp(-dt / MOMENTUM_TIME_CONSTANT_MS);
    momentumCarry += momentumVelocity * dt;

    const drained = drain(
      momentumCarry,
      Math.min(MAX_WHEELS_PER_FRAME, MAX_MOMENTUM_WHEELS - momentumWheelCount),
      lastClientX,
      lastClientY
    );
    momentumCarry = drained.carry;
    momentumWheelCount += drained.count;

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

  function drain(
    carry: number,
    budget: number,
    clientX: number,
    clientY: number
  ): { carry: number; count: number } {
    const direction: 1 | -1 = carry >= 0 ? 1 : -1;
    let count = 0;
    while (Math.abs(carry) >= stepPx && count < budget) {
      deps.dispatchWheel(direction, clientX, clientY);
      carry -= direction * stepPx;
      count += 1;
    }
    return { carry, count };
  }

  function getExitVelocity(releaseAt: number): number | undefined {
    if (velocitySamples.length < 2) return undefined;
    const newestIndex = velocitySamples.length - 1;
    const newest = velocitySamples[newestIndex];
    if (releaseAt - newest.t > MOMENTUM_RELEASE_MAX_AGE_MS) return undefined;

    // Estimate only the final consistent-direction run. A reversal is a new
    // intent, not noise to average together with the preceding drag. Sub-pixel
    // deltas are neutral and stay inside whichever run surrounds them.
    let oldestIndex = newestIndex;
    let direction = 0;
    for (let index = newestIndex; index > 0; index -= 1) {
      const delta = velocitySamples[index].y - velocitySamples[index - 1].y;
      const nextDirection =
        Math.abs(delta) < VELOCITY_DIRECTION_EPSILON_PX ? 0 : Math.sign(delta);
      if (nextDirection !== 0) {
        if (direction !== 0 && nextDirection !== direction) break;
        direction = nextDirection;
      }
      oldestIndex = index - 1;
    }
    if (direction === 0) return undefined;

    const oldest = velocitySamples[oldestIndex];
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
    onTouchStart(p: TouchPoint): TouchStartResult {
      if (p.touches === 1) {
        sequenceBeganAsCatch = momentumFrameId !== undefined;
      }
      verticalTravelOccurred = false;
      cancelMomentum();
      resetGesture(p.touches === 1 ? 'undecided' : 'ignore');
      stepPx = sanitizeWheelStep(deps.getLineHeightPx?.());
      startX = p.clientX;
      startY = p.clientY;
      lastClientX = p.clientX;
      lastClientY = p.clientY;

      if (p.touches === 1) {
        appendVelocitySample(p.timeStampMs ?? readNow(), p.clientY);
      }
      return { flingCatch: sequenceBeganAsCatch };
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
        if (axis === 'vertical') verticalTravelOccurred = true;
        if (axis !== 'vertical') return { prevent: false };
      }

      // Only bridge when xterm's own touch scroll is disabled (mouse tracking on).
      // Otherwise let xterm scroll its scrollback natively — avoids double-scroll.
      if (deps.getMouseTrackingMode() === 'none') return { prevent: false };

      const deltaY = lastClientY - p.clientY;
      lastClientX = p.clientX;
      lastClientY = p.clientY;
      appendVelocitySample(p.timeStampMs ?? readNow(), p.clientY);

      // Natural scrolling: finger up (clientY decreases) reveals newer content
      // (scroll down, +1); finger down reveals history (scroll up, -1).
      accumulated += deltaY;
      accumulated = drain(
        accumulated,
        Number.POSITIVE_INFINITY,
        p.clientX,
        p.clientY
      ).carry;

      // We've committed to the vertical gesture: always prevent page scroll/rubber-band,
      // even on sub-step moves that didn't dispatch a wheel yet.
      return { prevent: true };
    },

    onTouchEnd(remainingTouches = 0, timeStampMs?: number): TouchEndResult {
      cancelMomentum();

      const caughtFling =
        remainingTouches === 0 &&
        sequenceBeganAsCatch &&
        !verticalTravelOccurred;

      const exitVelocity =
        remainingTouches === 0 &&
        axis === 'vertical' &&
        !deps.isSuppressed?.() &&
        deps.getMouseTrackingMode() !== 'none'
          ? getExitVelocity(timeStampMs ?? readNow())
          : undefined;

      // Only re-arm once every finger is up; a partial lift from a pinch must
      // not let the remaining finger resume a bridged scroll from stale state.
      resetGesture(remainingTouches === 0 ? 'undecided' : 'ignore');

      if (exitVelocity !== undefined) {
        momentumVelocity = exitVelocity;
        momentumPrevT = readNow();
        scheduleMomentumFrame();
      }

      return { caughtFling };
    },

    onTouchCancel(remainingTouches = 0): void {
      cancelMomentum();
      resetGesture(remainingTouches === 0 ? 'undecided' : 'ignore');
    },
  };
}

/**
 * Wire the controller to a live xterm terminal's element. Attach ONCE per created
 * terminal (in the creation branch) and do NOT remove on React unmount — the
 * listeners live on `terminal.element` and tear down with it on
 * `terminal.dispose()`, mirroring the existing `contextmenu`/selection handlers.
 * `installTerminalTouchLayers` deliberately discards the disposer, so the
 * connectivity gate below enforces the detach/dispose leg of the cancellation
 * invariant: the next momentum tick stops before touching any terminal API.
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
  let scrollOwned = false;

  const controller = createTouchScrollController({
    getMouseTrackingMode: () =>
      el.isConnected ? terminal.modes.mouseTrackingMode : 'none',
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
      timeStampMs: e.timeStamp,
    };
  };

  const onStart = (e: TouchEvent) => {
    const { flingCatch } = controller.onTouchStart(toPoint(e));
    scrollOwned = false;
    patchTerminalMobileState(terminal, { flingCatch, scrollOwned });
  };
  const onMove = (e: TouchEvent) => {
    const { prevent } = controller.onTouchMove(toPoint(e));
    if (prevent && !scrollOwned) {
      scrollOwned = true;
      patchTerminalMobileState(terminal, { scrollOwned });
    }
    if (prevent && e.cancelable) {
      e.preventDefault();
    }
  };
  const onEnd = (e: TouchEvent) => {
    const { caughtFling } = controller.onTouchEnd(
      e.touches.length,
      e.timeStamp
    );
    if (caughtFling && e.cancelable) e.preventDefault();
  };
  const onCancel = (e: TouchEvent) =>
    controller.onTouchCancel(e.touches.length);

  el.addEventListener('touchstart', onStart, { passive: true });
  el.addEventListener('touchmove', onMove, { passive: false });
  el.addEventListener('touchend', onEnd, { passive: false });
  el.addEventListener('touchcancel', onCancel, { passive: true });

  return () => {
    controller.onTouchCancel();
    el.removeEventListener('touchstart', onStart);
    el.removeEventListener('touchmove', onMove);
    el.removeEventListener('touchend', onEnd);
    el.removeEventListener('touchcancel', onCancel);
  };
}
