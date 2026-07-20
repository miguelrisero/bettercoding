import { useSyncExternalStore } from 'react';

import { isTouchDevice } from './useIsMobile';

/**
 * Visible-viewport geometry tracking for the on-screen keyboard.
 *
 * iOS Safari never shrinks the layout viewport when the keyboard opens — a
 * `fixed inset-0` app keeps its full height and the keyboard just covers the
 * bottom half (including the terminal's input line). The only reliable signal
 * is `window.visualViewport`, so this store mirrors its height and vertical
 * offset. It still pins window scroll back to 0 when possible, but retaining
 * any residual iOS focus pan lets the app shell follow the visible viewport.
 *
 * Android Chrome resizes the layout viewport itself when the viewport meta
 * carries `interactive-widget=resizes-content`; there the visual height equals
 * the layout height and applying it is a no-op.
 *
 * Pinch-zoom is NOT the keyboard: when `visualViewport.scale > 1` the height
 * shrink and the offset pan are the user zooming, so the store reports null
 * (layout falls back to its full-size classes) and never fights the pan.
 *
 * Safari can also drop or misorder viewport events around keyboard dismissal
 * and app switching. Focus, visibility, and orientation settle checks re-read
 * the geometry after the keyboard animation so stale sizing self-heals.
 */

type Listener = () => void;

export type VisualViewportGeometry = {
  height: number | null;
  offsetTop: number;
};

/** Above this scale the viewport change is user pinch-zoom, not a keyboard. */
const ZOOM_SCALE_EPSILON = 1.01;
// iOS keyboard and browser-chrome animations settle at inconsistent times.
// Take an early shot for fast transitions and a late shot for slower ones.
const EARLY_VIEWPORT_SETTLE_MS = 250;
const LATE_VIEWPORT_SETTLE_MS = 700;

const FALLBACK_GEOMETRY: VisualViewportGeometry = {
  height: null,
  offsetTop: 0,
};

let listeners: Listener[] = [];
let snapshot = FALLBACK_GEOMETRY;
let initialized = false;
let attached = false;
let notifyFrame: number | null = null;

function readGeometry(): VisualViewportGeometry {
  const vv = window.visualViewport;
  if (!vv || vv.scale > ZOOM_SCALE_EPSILON) return FALLBACK_GEOMETRY;

  // Round down so the container never overflows the visible area by a
  // sub-pixel (which would put the terminal's last row behind the keyboard).
  return {
    height: Math.floor(vv.height),
    offsetTop: Math.floor(vv.offsetTop),
  };
}

function updateSnapshot(next: VisualViewportGeometry) {
  initialized = true;
  if (
    next.height === snapshot.height &&
    next.offsetTop === snapshot.offsetTop
  ) {
    return;
  }

  snapshot = next;
  // Coalesce the keyboard-animation event burst to one notify per frame —
  // this store re-renders the app shell, so per-event renders are real cost.
  if (notifyFrame !== null) return;
  notifyFrame = requestAnimationFrame(() => {
    notifyFrame = null;
    for (const l of [...listeners]) l();
  });
}

function onViewportChange() {
  const vv = window.visualViewport;
  // iOS pans the visual viewport to reveal a focused input near the bottom.
  // Undo layout scrolling when possible, then retain any residual visual pan
  // so the fixed app shell can follow what the user can actually see. Never
  // fight an offset while zoomed, which belongs to the user's pinch gesture.
  if (vv && vv.offsetTop > 0 && vv.scale <= ZOOM_SCALE_EPSILON) {
    window.scrollTo(0, 0);
  }
  updateSnapshot(readGeometry());
}

function scheduleViewportSettleResync() {
  setTimeout(onViewportChange, EARLY_VIEWPORT_SETTLE_MS);
  setTimeout(onViewportChange, LATE_VIEWPORT_SETTLE_MS);
}

function subscribe(listener: Listener) {
  listeners.push(listener);
  const vv = window.visualViewport;
  // Never attach on non-touch devices — the hook always snapshots null there,
  // so listening would only risk desktop side effects for no consumer.
  // (Touch capability is session-constant; see isTouchDevice.)
  if (vv && !attached && isTouchDevice()) {
    attached = true;
    onViewportChange();
    vv.addEventListener('resize', onViewportChange);
    vv.addEventListener('scroll', onViewportChange);
    window.addEventListener('focusout', scheduleViewportSettleResync);
    document.addEventListener('visibilitychange', scheduleViewportSettleResync);
    window.addEventListener('orientationchange', scheduleViewportSettleResync);
  }
  return () => {
    listeners = listeners.filter((l) => l !== listener);
    // Viewport, window, and document listeners stay attached for the session —
    // the store is module-level and the events are cheap; detaching/reattaching
    // on every subscriber churn buys nothing.
  };
}

function getSnapshot(): VisualViewportGeometry {
  if (!isTouchDevice()) return FALLBACK_GEOMETRY;
  if (!initialized) {
    snapshot = readGeometry();
    initialized = true;
  }
  return snapshot;
}

/**
 * Current visual viewport geometry on touch devices. Height is `null` and the
 * offset is zero when the device isn't touch-capable, `visualViewport` is
 * unavailable, or the user is pinch-zoomed — callers fall back to their normal
 * (layout-viewport) sizing on `null`.
 */
export function useVisualViewportGeometry(): VisualViewportGeometry {
  return useSyncExternalStore(subscribe, getSnapshot, () => FALLBACK_GEOMETRY);
}

// Test-only escape hatch for exercising the attach-once module store without a
// DOM renderer. Production consumers should use useVisualViewportGeometry.
export const __vvStoreForTests = { subscribe, getSnapshot };
