import { useState, useSyncExternalStore } from 'react';

const MOBILE_BREAKPOINT = 767;
const query = `(max-width: ${MOBILE_BREAKPOINT}px)`;
let mediaQuery: MediaQueryList | null = null;

function getMediaQuery() {
  if (!mediaQuery) {
    mediaQuery = window.matchMedia(query);
  }
  return mediaQuery;
}

function subscribe(callback: () => void) {
  const mq = getMediaQuery();
  mq.addEventListener('change', callback);
  return () => mq.removeEventListener('change', callback);
}

function getSnapshot() {
  return getMediaQuery().matches;
}

/**
 * Returns true when the viewport is at or below mobile breakpoint (767px).
 * Uses a singleton MediaQueryList for efficient re-renders.
 */
export function useIsMobile(): boolean {
  return useSyncExternalStore(subscribe, getSnapshot);
}

/** Raw check without React hook — for use in store actions */
export function isMobileViewport(): boolean {
  return window.matchMedia(query).matches;
}

/** Detect real mobile device via user-agent (not just viewport width) */
export function isRealMobileDevice(): boolean {
  // Modern API: navigator.userAgentData.mobile (Chrome, Edge, Opera — ~76% of browsers)
  const nav = navigator as Navigator & { userAgentData?: { mobile?: boolean } };
  if (nav.userAgentData?.mobile !== undefined) {
    return nav.userAgentData.mobile;
  }
  // Fallback: user-agent string regex (Safari, Firefox)
  return /Android|webOS|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini|Windows Phone|Mobi/i.test(
    navigator.userAgent
  );
}

/** React hook version of isRealMobileDevice — stable, no re-renders on resize */
export function useIsRealMobile(): boolean {
  // Device type doesn't change during session, so compute once
  const [isReal] = useState(() => isRealMobileDevice());
  return isReal;
}

/**
 * Capability-based touch detection (coarse pointer OR touch points).
 *
 * Distinct from the viewport (`useIsMobile`) and user-agent (`useIsRealMobile`)
 * detectors above: this answers "can this device touch?", which also covers
 * iPadOS with a desktop UA and hybrid touch laptops. Capability doesn't change
 * within a session, so it is computed once.
 */
export function isTouchDevice(): boolean {
  if (typeof window === 'undefined') return false;
  const coarsePointer =
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(pointer: coarse)').matches;
  return coarsePointer || navigator.maxTouchPoints > 0;
}

/** React hook version of isTouchDevice — stable, computed once. */
export function useIsTouchDevice(): boolean {
  const [touch] = useState(isTouchDevice);
  return touch;
}
