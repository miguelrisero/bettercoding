/**
 * Conversation Virtualizer Hook
 *
 * Shared TanStack Virtual configuration for the conversation list.
 * Owns the virtualizer instance, measurement, and imperative scroll helpers.
 */

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type RefObject,
} from 'react';
import {
  useVirtualizer,
  measureElement as defaultMeasureElement,
} from '@tanstack/react-virtual';
import type { Virtualizer, VirtualItem } from '@tanstack/react-virtual';

import {
  type ConversationRow,
  SIZE_ESTIMATE_PX,
  estimateSizeForRow,
  findPreviousUserMessageIndex,
} from './conversation-row-model';
import {
  isNearBottom,
  shouldReleaseBottomLock,
} from './conversation-scroll-commands';
import {
  scrollDebug,
  installScrollProbe,
  tagScrollWrite,
} from './conversation-scroll-debug';

// TanStack Virtual's ScrollBehavior ('auto' | 'smooth' | 'instant') shadows
// the DOM ScrollBehavior. Use a narrow type to avoid TS2322 mismatches.
type ScrollToOptionsBehavior = 'auto' | 'smooth';

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Number of items to render beyond the visible area in each direction. */
const OVERSCAN = 8;

/**
 * How long after a direct user scroll input (wheel/touch/keyboard) the
 * virtualizer's size-change scroll compensation stays suppressed.
 *
 * While history loads, freshly mounted rows re-measure away from their
 * estimates and each above-viewport correction rewrites scroll position via
 * `scrollToFn`. If the user is actively scrolling at that moment, those
 * corrections fight the gesture (observed live: 84 forced scrollTo calls in
 * 8s dragging the view ~2.6k px against the wheel). User input must win;
 * anchoring resumes once the gesture pauses.
 */
const USER_SCROLL_INPUT_PRIORITY_MS = 400;

/** Keys that scroll the conversation when it has focus. */
const SCROLL_KEYS = new Set([
  'ArrowUp',
  'ArrowDown',
  'PageUp',
  'PageDown',
  'Home',
  'End',
  ' ',
]);

/** Subset of SCROLL_KEYS that move the viewport up (away from the bottom). */
const SCROLL_UP_KEYS = new Set(['ArrowUp', 'PageUp', 'Home']);

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface ConversationVirtualizerOptions {
  /** The semantic row model driving the list (virtualized head only). */
  rows: ConversationRow[];

  /**
   * Total number of conversation rows (virtualized + unvirtualized tail).
   * The bottom-lock correction must fire when ANY row is added — including
   * unvirtualized tail rows that don't change `rows.length` or `totalSize`.
   * Without this, streaming entries appended to the tail silently grow the
   * scroll container while the correction never fires.
   */
  totalRowCount: number;

  /** Ref to the scrollable container element. */
  scrollContainerRef: RefObject<HTMLDivElement | null>;

  /**
   * Called when the at-bottom state changes. Shells use this to show/hide
   * the scroll-to-bottom affordance.
   */
  onAtBottomChange?: (atBottom: boolean) => void;

  shouldSuppressSizeAdjustment?: () => boolean;
}

export interface ConversationVirtualizerResult {
  /** The TanStack Virtual virtualizer instance. */
  virtualizer: Virtualizer<HTMLDivElement, Element>;

  /** Virtual items currently in the render window (including overscan). */
  virtualItems: VirtualItem[];

  /** Total pixel size of all items (for the scroll spacer). */
  totalSize: number;

  /**
   * Ref callback for row DOM elements. Attach to each rendered row's
   * container element alongside `data-index={virtualItem.index}`.
   * TanStack Virtual uses this to measure real DOM heights and attach
   * a ResizeObserver for automatic re-measurement on size changes.
   */
  measureElement: (node: Element | null) => void;

  /** Scroll to the absolute bottom of the list. */
  scrollToBottom: (behavior?: ScrollToOptionsBehavior) => void;

  /** Scroll to a specific row index. */
  scrollToIndex: (
    index: number,
    options?: {
      align?: 'start' | 'center' | 'end';
      behavior?: ScrollToOptionsBehavior;
    }
  ) => void;

  /**
   * Scroll to the previous user message relative to the first visible item.
   * Returns true if a target was found and scrolled to, false otherwise.
   */
  scrollToPreviousUserMessage: () => boolean;

  /**
   * Whether the scroll container is currently near the bottom.
   * Reactive — updates via scroll event listener, not just point-in-time.
   */
  isAtBottom: boolean;

  /** Point-in-time check (non-reactive). Reads DOM directly. */
  checkIsAtBottom: () => boolean;

  /**
   * Release the bottom-lock. Call when navigating away from the
   * bottom (e.g., scrollToPreviousUserMessage).
   */
  releaseBottomLock: () => void;

  /**
   * Whether the user issued a direct scroll input (wheel/touch/keys) within
   * the last `USER_SCROLL_INPUT_PRIORITY_MS`. Consumers use this to avoid
   * hijacking an actively-reading user (e.g. a stale follow-bottom intent).
   */
  isUserScrollInputRecent: () => boolean;

  /**
   * Look up the ConversationRow index for a given virtual item.
   * Since our virtualizer uses identity mapping (no lane reordering),
   * this is simply `virtualItem.index`.
   */
  rowIndexForVirtualItem: (item: VirtualItem) => number;

  /**
   * Look up the ConversationRow for a given virtual item.
   * Returns undefined if the index is out of bounds.
   */
  rowForVirtualItem: (item: VirtualItem) => ConversationRow | undefined;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/**
 * Configure and return a TanStack Virtual virtualizer for the conversation list.
 *
 * This hook is the single source of virtualizer configuration. It is consumed
 * by `ConversationListContainer` and must not be duplicated across shells.
 */
export function useConversationVirtualizer({
  rows,
  totalRowCount,
  scrollContainerRef,
  onAtBottomChange,
  shouldSuppressSizeAdjustment,
}: ConversationVirtualizerOptions): ConversationVirtualizerResult {
  const bottomLockedRef = useRef(false);
  const smoothScrollDeadlineRef = useRef(0);
  /** Timestamp of the last direct user scroll input (wheel/touch/keys). */
  const lastUserScrollInputRef = useRef(0);
  /**
   * Live container width for size estimation. `clientWidth` reads 0 before
   * layout, which makes every first-paint estimate wrong and inflates the
   * re-measurement storm while history loads.
   */
  const containerWidthRef = useRef<number | null>(null);

  /** Latest rows, readable from callbacks without re-subscribing. */
  const rowsRef = useRef(rows);
  rowsRef.current = rows;

  /**
   * The row pinned to a fixed screen position while the user reads history
   * during load. `visualTop` is the row's top offset relative to the
   * container's top (`rowStart - scrollTop`); holding it constant across
   * re-measures and prepends keeps the content under the reader's eyes still.
   */
  const viewportAnchorRef = useRef<{
    key: string | number;
    index: number;
    visualTop: number;
  } | null>(null);

  const isBottomScrollCorrectionActive = useCallback(
    () => bottomLockedRef.current,
    []
  );

  const isUserScrollInputRecent = useCallback(
    () =>
      performance.now() - lastUserScrollInputRef.current <
      USER_SCROLL_INPUT_PRIORITY_MS,
    []
  );

  useEffect(() => {
    const el = scrollContainerRef.current;
    if (!el) return;

    // A direct upward gesture is an unambiguous intent to leave the bottom, so
    // release the bottom-lock straight from the input event. This is the one
    // signal the load-time re-pin can't corrupt: wheel/key direction is read
    // before any scroll write. Inferring the release from scrollTop deltas
    // (shouldReleaseBottomLock) fails while history streams in, because the
    // re-pin overwrites the very delta it would read — the user's upward wheel
    // nets out as a downward move once we slam back to the bottom, so the fight
    // only ends when loading stops. Releasing here breaks that cycle at its
    // source and keeps checkIsAtBottom honest afterward (no re-pin → no false
    // "at bottom" → no auto-relock).
    const markUserScroll = (movesAwayFromBottom: boolean) => {
      lastUserScrollInputRef.current = performance.now();
      if (movesAwayFromBottom && bottomLockedRef.current) {
        bottomLockedRef.current = false;
        scrollDebug('release:user-up', { scrollTop: el.scrollTop });
      }
    };
    const onWheel = (event: WheelEvent) => markUserScroll(event.deltaY < 0);
    // Touch direction isn't known cheaply here; the re-pin gate and scroll
    // handler cover touch via their (now un-corrupted) scrollTop reads.
    const onTouchMove = () => markUserScroll(false);
    const onKeyDown = (event: KeyboardEvent) => {
      if (SCROLL_KEYS.has(event.key)) {
        markUserScroll(SCROLL_UP_KEYS.has(event.key));
      }
    };

    el.addEventListener('wheel', onWheel, { passive: true });
    el.addEventListener('touchmove', onTouchMove, { passive: true });
    el.addEventListener('keydown', onKeyDown);

    // Diagnostics: capture every programmatic scroll write on the container so a
    // real-world reproduction shows which writer fights the user.
    const uninstallProbe = installScrollProbe(el);

    containerWidthRef.current = el.clientWidth || null;
    const resizeObserver = new ResizeObserver(() => {
      containerWidthRef.current = el.clientWidth || null;
    });
    resizeObserver.observe(el);

    return () => {
      el.removeEventListener('wheel', onWheel);
      el.removeEventListener('touchmove', onTouchMove);
      el.removeEventListener('keydown', onKeyDown);
      resizeObserver.disconnect();
      uninstallProbe();
    };
  }, [scrollContainerRef]);

  // -------------------------------------------------------------------------
  // Virtualizer instance
  // -------------------------------------------------------------------------

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize: (index) => {
      const row = rows[index];
      if (!row) return SIZE_ESTIMATE_PX.medium;
      const containerWidth =
        containerWidthRef.current ??
        scrollContainerRef.current?.clientWidth ??
        null;
      return estimateSizeForRow(row, containerWidth);
    },
    getItemKey: (index) => {
      const row = rows[index];
      return row ? row.semanticKey : index;
    },
    overscan: OVERSCAN,
    measureElement: defaultMeasureElement,
    useAnimationFrameWithResizeObserver: false,
  });

  // -------------------------------------------------------------------------
  // shouldAdjustScrollPositionOnItemSizeChange — DISABLED
  //
  // TanStack's built-in per-item compensation fires once for every row that
  // re-measures above the viewport. While a long history streams in that is
  // dozens of separate `scrollTo` nudges (measured live: 46 writes / ~900px of
  // small, alternating corrections) — the user perceives them as the view
  // "randomly scrolling up and down" until measurement settles. We replace it
  // with a single synchronous viewport anchor (below) that holds one visible
  // row at a fixed screen position across an entire render, so re-measures and
  // prepends never move what the reader is looking at.
  // -------------------------------------------------------------------------

  useEffect(() => {
    virtualizer.shouldAdjustScrollPositionOnItemSizeChange = () => false;
    return () => {
      virtualizer.shouldAdjustScrollPositionOnItemSizeChange = undefined;
    };
  }, [virtualizer]);

  // -------------------------------------------------------------------------
  // Viewport anchoring (replaces TanStack's per-item compensation)
  // -------------------------------------------------------------------------

  /** Record the first visible row and where it sits, to restore after renders. */
  const captureViewportAnchor = useCallback(() => {
    const el = scrollContainerRef.current;
    if (!el || bottomLockedRef.current) {
      viewportAnchorRef.current = null;
      return;
    }
    const scrollTop = el.scrollTop;
    const items = virtualizer.getVirtualItems();
    const first = items.find((item) => item.end > scrollTop) ?? items[0];
    const row = first ? rowsRef.current[first.index] : undefined;
    viewportAnchorRef.current =
      first && row
        ? {
            key: row.semanticKey,
            index: first.index,
            visualTop: first.start - scrollTop,
          }
        : null;
  }, [scrollContainerRef, virtualizer]);

  // -------------------------------------------------------------------------
  // Reactive isAtBottom state
  // -------------------------------------------------------------------------

  const [isAtBottomState, setIsAtBottomState] = useState(true);
  const onAtBottomChangeRef = useRef(onAtBottomChange);
  onAtBottomChangeRef.current = onAtBottomChange;
  const lastAtBottomRef = useRef(true);

  const syncIsAtBottom = useCallback(() => {
    const el = scrollContainerRef.current;
    const nextValue = isBottomScrollCorrectionActive()
      ? true
      : el
        ? isNearBottom(el.scrollTop, el.clientHeight, el.scrollHeight)
        : true;

    if (nextValue !== lastAtBottomRef.current) {
      lastAtBottomRef.current = nextValue;
      setIsAtBottomState(nextValue);
      onAtBottomChangeRef.current?.(nextValue);
      return;
    }

    setIsAtBottomState((current) =>
      current === nextValue ? current : nextValue
    );
  }, [isBottomScrollCorrectionActive, scrollContainerRef]);

  const prevScrollTopRef = useRef(0);
  const prevScrollHeightRef = useRef(0);

  useEffect(() => {
    const el = scrollContainerRef.current;
    if (!el) return;

    prevScrollTopRef.current = el.scrollTop;
    prevScrollHeightRef.current = el.scrollHeight;

    const handleScroll = () => {
      const currentScrollTop = el.scrollTop;
      const currentScrollHeight = el.scrollHeight;

      // Release the bottom lock only on a genuine user scroll-up. The release
      // decision is centralized in shouldReleaseBottomLock, which also ignores
      // content-driven upward moves (shrinking scrollHeight from re-measured
      // rows / browser clamps) — those would otherwise spuriously release the
      // lock while history streams in and cause scroll oscillation.
      if (
        shouldReleaseBottomLock({
          bottomLocked: bottomLockedRef.current,
          prevScrollTop: prevScrollTopRef.current,
          currentScrollTop,
          prevScrollHeight: prevScrollHeightRef.current,
          currentScrollHeight,
          withinProgrammaticScroll:
            performance.now() <= smoothScrollDeadlineRef.current,
          sizeAdjustmentActive: shouldSuppressSizeAdjustment?.() ?? false,
        })
      ) {
        bottomLockedRef.current = false;
        scrollDebug('release:scroll-heuristic', {
          scrollTop: currentScrollTop,
        });
      }

      prevScrollTopRef.current = currentScrollTop;
      prevScrollHeightRef.current = currentScrollHeight;
      syncIsAtBottom();
      // Re-anchor to whatever the user just scrolled to, so the next render's
      // restore holds this position.
      captureViewportAnchor();
    };

    el.addEventListener('scroll', handleScroll, { passive: true });
    handleScroll();

    return () => {
      el.removeEventListener('scroll', handleScroll);
    };
  }, [
    scrollContainerRef,
    shouldSuppressSizeAdjustment,
    syncIsAtBottom,
    captureViewportAnchor,
  ]);

  // -------------------------------------------------------------------------
  // Derived state
  // -------------------------------------------------------------------------

  const virtualItems = virtualizer.getVirtualItems();
  const totalSize = virtualizer.getTotalSize();

  // Viewport-anchor restore. Runs synchronously after every render that changed
  // measurements (rows/totalSize deps), before paint, so a re-measure or
  // prepend above the reader never shifts what they see. One scroll write per
  // render, anchored to a real visible row — the smooth replacement for
  // TanStack's many small per-item nudges. Skipped while bottom-locked (the
  // re-pin owns position then) and during programmatic/interaction scrolls.
  useLayoutEffect(() => {
    const el = scrollContainerRef.current;
    const anchor = viewportAnchorRef.current;

    if (
      el &&
      anchor &&
      !bottomLockedRef.current &&
      performance.now() >= smoothScrollDeadlineRef.current &&
      !shouldSuppressSizeAdjustment?.()
    ) {
      // Prepends shift the anchor row to a higher index; fast-path the common
      // no-prepend case, else find it by stable semantic key.
      let index =
        rows[anchor.index]?.semanticKey === anchor.key ? anchor.index : -1;
      if (index === -1) {
        index = rows.findIndex((r) => r.semanticKey === anchor.key);
      }
      if (index !== -1) {
        const start = virtualizer.measurementsCache[index]?.start;
        if (start !== undefined) {
          const target = start - anchor.visualTop;
          if (target >= 0 && Math.abs(target - el.scrollTop) > 0.5) {
            tagScrollWrite('viewport-anchor');
            el.scrollTop = target;
          }
        }
      }
    }

    captureViewportAnchor();
  }, [
    rows,
    totalRowCount,
    totalSize,
    virtualizer,
    scrollContainerRef,
    shouldSuppressSizeAdjustment,
    captureViewportAnchor,
  ]);

  useLayoutEffect(() => {
    syncIsAtBottom();

    if (!bottomLockedRef.current) return;
    if (performance.now() < smoothScrollDeadlineRef.current) return;

    const el = scrollContainerRef.current;
    if (!el) return;

    // User input wins. While history streams in, this re-pin runs on every
    // batch; firing it against an actively-scrolling user yanks them back to
    // the bottom every frame — the load-time "random scroll" fight. A recent
    // gesture that has left the bottom releases the lock here instead. This is
    // the backstop for inputs the wheel-direction release can't classify
    // (touch), and for any path that re-armed the lock mid-gesture.
    if (
      isUserScrollInputRecent() &&
      !isNearBottom(el.scrollTop, el.clientHeight, el.scrollHeight)
    ) {
      bottomLockedRef.current = false;
      scrollDebug('release:repin-gate', { scrollTop: el.scrollTop });
      syncIsAtBottom();
      return;
    }

    const maxScroll = el.scrollHeight - el.clientHeight;
    if (maxScroll > 0 && Math.abs(maxScroll - el.scrollTop) > 1) {
      // `fighting` = re-pinning to bottom while the user is actively scrolling
      // away from it. With the release paths above this should never be true;
      // if it shows up in a real session, the lock failed to release.
      scrollDebug('repin', {
        from: Math.round(el.scrollTop),
        to: Math.round(maxScroll),
        fighting:
          isUserScrollInputRecent() &&
          !isNearBottom(el.scrollTop, el.clientHeight, el.scrollHeight),
      });
      tagScrollWrite('repin');
      el.scrollTop = maxScroll;
    }
  }, [
    rows.length,
    totalRowCount,
    totalSize,
    syncIsAtBottom,
    scrollContainerRef,
    isUserScrollInputRecent,
  ]);

  // -------------------------------------------------------------------------
  // Imperative helpers
  // -------------------------------------------------------------------------

  const scrollToBottom = useCallback(
    (behavior: ScrollToOptionsBehavior = 'smooth') => {
      const el = scrollContainerRef.current;
      if (!el) return;

      if (!bottomLockedRef.current) {
        scrollDebug('lock:acquire', { behavior, scrollTop: el.scrollTop });
      }
      bottomLockedRef.current = true;

      if (behavior === 'smooth') {
        smoothScrollDeadlineRef.current = performance.now() + 500;
        tagScrollWrite('scroll-to-bottom');
        el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' });
      } else {
        tagScrollWrite('scroll-to-bottom');
        el.scrollTop = el.scrollHeight - el.clientHeight;
      }
    },
    [scrollContainerRef, virtualizer]
  );

  const scrollToIndex = useCallback(
    (
      index: number,
      options?: {
        align?: 'start' | 'center' | 'end';
        behavior?: ScrollToOptionsBehavior;
      }
    ) => {
      if (bottomLockedRef.current) {
        bottomLockedRef.current = false;
      }

      virtualizer.scrollToIndex(index, {
        align: options?.align ?? 'start',
        behavior: options?.behavior ?? 'smooth',
      });
    },
    [virtualizer]
  );

  const scrollToPreviousUserMessage = useCallback((): boolean => {
    const scrollEl = scrollContainerRef.current;
    const items = virtualizer.getVirtualItems();
    if (items.length === 0 || rows.length === 0 || !scrollEl) return false;

    const firstVisibleIndex =
      virtualizer.getVirtualItemForOffset(scrollEl.scrollTop)?.index ??
      items[0].index;
    const targetIndex = findPreviousUserMessageIndex(rows, firstVisibleIndex);

    if (targetIndex < 0) return false;

    virtualizer.scrollToIndex(targetIndex, {
      align: 'start',
      behavior: 'smooth',
    });
    return true;
  }, [scrollContainerRef, virtualizer, rows]);

  const checkIsAtBottom = useCallback((): boolean => {
    const el = scrollContainerRef.current;
    if (!el) return true;
    return isNearBottom(el.scrollTop, el.clientHeight, el.scrollHeight);
  }, [scrollContainerRef]);

  const releaseBottomLock = useCallback(() => {
    if (!bottomLockedRef.current) return;
    bottomLockedRef.current = false;
  }, []);

  // -------------------------------------------------------------------------
  // Row ↔ VirtualItem mapping
  // -------------------------------------------------------------------------

  const rowIndexForVirtualItem = useCallback(
    (item: VirtualItem): number => item.index,
    []
  );

  const rowForVirtualItem = useCallback(
    (item: VirtualItem): ConversationRow | undefined => rows[item.index],
    [rows]
  );

  const measureElement = useCallback(
    (node: Element | null) => {
      virtualizer.measureElement(node);
    },
    [virtualizer]
  );

  // -------------------------------------------------------------------------
  // Return
  // -------------------------------------------------------------------------

  return {
    virtualizer,
    virtualItems,
    totalSize,
    measureElement,
    scrollToBottom,
    scrollToIndex,
    scrollToPreviousUserMessage,
    isAtBottom: isAtBottomState,
    checkIsAtBottom,
    releaseBottomLock,
    isUserScrollInputRecent,
    rowIndexForVirtualItem,
    rowForVirtualItem,
  };
}
