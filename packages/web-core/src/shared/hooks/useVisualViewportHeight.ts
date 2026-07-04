import { useSyncExternalStore } from 'react';

import { isTouchDevice } from './useIsMobile';

/**
 * Visible-viewport height tracking for the on-screen keyboard.
 *
 * iOS Safari never shrinks the layout viewport when the keyboard opens — a
 * `fixed inset-0` app keeps its full height and the keyboard just covers the
 * bottom half (including the terminal's input line). The only reliable signal
 * is `window.visualViewport`, so this store mirrors its height (and pins the
 * window scroll back to 0 when iOS pans the page on focus).
 *
 * Android Chrome resizes the layout viewport itself when the viewport meta
 * carries `interactive-widget=resizes-content`; there the visual height equals
 * the layout height and applying it is a no-op.
 *
 * Pinch-zoom is NOT the keyboard: when `visualViewport.scale > 1` the height
 * shrink and the offset pan are the user zooming, so the store reports null
 * (layout falls back to its full-size classes) and never fights the pan.
 */

type Listener = () => void;

/** Above this scale the viewport change is user pinch-zoom, not a keyboard. */
const ZOOM_SCALE_EPSILON = 1.01;

let listeners: Listener[] = [];
let height: number | null = null;
let attached = false;
let notifyFrame: number | null = null;

function readHeight(): number | null {
  const vv = window.visualViewport;
  if (!vv) return null;
  if (vv.scale > ZOOM_SCALE_EPSILON) return null;
  // Round down so the container never overflows the visible area by a
  // sub-pixel (which would put the terminal's last row behind the keyboard).
  return Math.floor(vv.height);
}

function onViewportChange() {
  const vv = window.visualViewport;
  if (!vv) return;
  // iOS pans the visual viewport to reveal a focused input near the bottom;
  // with the app sized to the visible area there is nothing to reveal, so
  // undo the pan to keep the app chrome pinned to the top. Never at zoom —
  // an offset while zoomed is the user panning around their pinch-zoom.
  if (vv.offsetTop > 0 && vv.scale <= ZOOM_SCALE_EPSILON) {
    window.scrollTo(0, 0);
  }
  const next = readHeight();
  if (next === height) return;
  height = next;
  // Coalesce the keyboard-animation event burst to one notify per frame —
  // this store re-renders the app shell, so per-event renders are real cost.
  if (notifyFrame !== null) return;
  notifyFrame = requestAnimationFrame(() => {
    notifyFrame = null;
    for (const l of [...listeners]) l();
  });
}

function subscribe(listener: Listener) {
  listeners.push(listener);
  const vv = window.visualViewport;
  // Never attach on non-touch devices — the hook always snapshots null there,
  // so listening would only risk desktop side effects for no consumer.
  // (Touch capability is session-constant; see isTouchDevice.)
  if (vv && !attached && isTouchDevice()) {
    attached = true;
    height = readHeight();
    vv.addEventListener('resize', onViewportChange);
    vv.addEventListener('scroll', onViewportChange);
  }
  return () => {
    listeners = listeners.filter((l) => l !== listener);
    // Listeners on visualViewport stay attached for the session — the store is
    // module-level and the events are cheap; detaching/reattaching on every
    // subscriber churn buys nothing.
  };
}

function getSnapshot(): number | null {
  if (!isTouchDevice()) return null;
  if (height === null) height = readHeight();
  return height;
}

/**
 * Current visual viewport height in px on touch devices, or `null` when the
 * device isn't touch-capable, `visualViewport` is unavailable, or the user is
 * pinch-zoomed — callers fall back to their normal (layout-viewport) sizing
 * on `null`.
 */
export function useVisualViewportHeight(): number | null {
  return useSyncExternalStore(subscribe, getSnapshot, () => null);
}
