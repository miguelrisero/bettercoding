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
 */

type Listener = () => void;

let listeners: Listener[] = [];
let height: number | null = null;
let attached = false;

function readHeight(): number | null {
  const vv = window.visualViewport;
  if (!vv) return null;
  // Round down so the container never overflows the visible area by a
  // sub-pixel (which would put the terminal's last row behind the keyboard).
  return Math.floor(vv.height);
}

function onViewportChange() {
  // iOS pans the visual viewport to reveal a focused input near the bottom;
  // with the app sized to the visible area there is nothing to reveal, so
  // undo the pan to keep the app chrome pinned to the top.
  if (window.visualViewport && window.visualViewport.offsetTop > 0) {
    window.scrollTo(0, 0);
  }
  const next = readHeight();
  if (next === height) return;
  height = next;
  for (const l of [...listeners]) l();
}

function subscribe(listener: Listener) {
  listeners.push(listener);
  const vv = window.visualViewport;
  if (vv && !attached) {
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
 * device isn't touch-capable or `visualViewport` is unavailable — callers fall
 * back to their normal (layout-viewport) sizing on `null`.
 */
export function useVisualViewportHeight(): number | null {
  return useSyncExternalStore(subscribe, getSnapshot, () => null);
}
