import { useState } from 'react';

/**
 * Capability-based touch detection (coarse pointer OR touch points).
 *
 * Unlike `useIsRealMobile()` (user-agent based), this also covers iPadOS with a
 * desktop UA and hybrid touch laptops — the population that needs the on-screen
 * terminal controls. Device capability does not change within a session, so it
 * is computed once.
 */
export function isTouchDevice(): boolean {
  if (typeof window === 'undefined') return false;
  const coarsePointer =
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(pointer: coarse)').matches;
  return coarsePointer || navigator.maxTouchPoints > 0;
}

/** React hook version — stable, computed once (no re-render on resize). */
export function useIsTouchDevice(): boolean {
  const [touch] = useState(isTouchDevice);
  return touch;
}
