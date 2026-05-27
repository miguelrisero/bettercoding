/**
 * Conversation Scroll Commands
 *
 * Declarative scroll intent model for the conversation list.
 */

import type { AddEntryType } from '@/shared/hooks/useConversationHistory/types';

// ---------------------------------------------------------------------------
// Near-Bottom Threshold
// ---------------------------------------------------------------------------

/**
 * Pixel distance from bottom within which the user is considered "at bottom".
 * Accounts for sub-pixel rounding, scroll inertia, and minor content growth.
 */
export const NEAR_BOTTOM_THRESHOLD_PX = 64;

// ---------------------------------------------------------------------------
// Scroll Intent
// ---------------------------------------------------------------------------

/**
 * Jump to bottom on first load and invalidate estimated sizes.
 */
export interface InitialBottomIntent {
  readonly type: 'initial-bottom';
  readonly purgeEstimatedSizes: true;
}

/**
 * Follow the bottom while streaming if the user is already there.
 */
export interface FollowBottomIntent {
  readonly type: 'follow-bottom';
  readonly behavior: ScrollBehavior;
}

/**
 * Preserve the current viewport while history changes above it.
 */
export interface PreserveAnchorIntent {
  readonly type: 'preserve-anchor';
}

/** Scroll so the last item's top is visible (plan presentation). */
export interface PlanRevealIntent {
  readonly type: 'plan-reveal';
  readonly align: 'start';
}

/** Explicit user action to return to bottom (scroll-to-bottom button). */
export interface JumpToBottomIntent {
  readonly type: 'jump-to-bottom';
  readonly behavior: ScrollBehavior;
}

/** Scroll to a specific row index (previous-user-message, jump-to-item). */
export interface JumpToIndexIntent {
  readonly type: 'jump-to-index';
  readonly index: number;
  readonly align: 'start' | 'center' | 'end';
  readonly behavior: ScrollBehavior;
}

export type ScrollIntent =
  | InitialBottomIntent
  | FollowBottomIntent
  | PreserveAnchorIntent
  | PlanRevealIntent
  | JumpToBottomIntent
  | JumpToIndexIntent;

// ---------------------------------------------------------------------------
// Scroll State
// ---------------------------------------------------------------------------

/**
 * Single source of truth for conversation scroll behaviour.
 */
export interface ScrollState {
  /** Whether the user is at (or near) the bottom of the list. */
  readonly isAtBottom: boolean;

  /** Intent waiting to be applied after virtualizer measurement. */
  readonly pendingIntent: ScrollIntent | null;

  /** Last successfully applied intent (for deduplication). */
  readonly lastAppliedIntent: ScrollIntent | null;
}

// ---------------------------------------------------------------------------
// State Factory
// ---------------------------------------------------------------------------

export function createInitialScrollState(): ScrollState {
  return {
    isAtBottom: true,
    pendingIntent: null,
    lastAppliedIntent: null,
  };
}

// ---------------------------------------------------------------------------
// Intent Resolution
// ---------------------------------------------------------------------------

/**
 * Map a data update to the scroll intent that should be applied next.
 */
export function resolveScrollIntent(
  addType: AddEntryType,
  isInitialLoad: boolean,
  isAtBottom: boolean
): ScrollIntent {
  if (isInitialLoad) {
    return { type: 'initial-bottom', purgeEstimatedSizes: true };
  }

  if (addType === 'plan') {
    return isAtBottom
      ? { type: 'plan-reveal', align: 'start' }
      : { type: 'preserve-anchor' };
  }

  if (addType === 'running') {
    return isAtBottom
      ? { type: 'follow-bottom', behavior: 'auto' }
      : { type: 'preserve-anchor' };
  }

  return isAtBottom
    ? { type: 'follow-bottom', behavior: 'auto' }
    : { type: 'preserve-anchor' };
}

// ---------------------------------------------------------------------------
// Auto-Follow Predicate
// ---------------------------------------------------------------------------

/**
 * Whether a new update should auto-follow the bottom.
 */
export function shouldAutoFollow(
  state: ScrollState,
  addType: AddEntryType
): boolean {
  if (!state.isAtBottom) return false;
  if (addType === 'plan') return false;
  return true;
}

// ---------------------------------------------------------------------------
// State Transitions
// ---------------------------------------------------------------------------

/** Set a new pending intent, replacing any existing one. */
export function setPendingIntent(
  state: ScrollState,
  intent: ScrollIntent
): ScrollState {
  return { ...state, pendingIntent: intent };
}

/** Mark pending intent as applied and move it to `lastAppliedIntent`. */
export function markIntentApplied(state: ScrollState): ScrollState {
  return {
    ...state,
    lastAppliedIntent: state.pendingIntent,
    pendingIntent: null,
  };
}

/** Update `isAtBottom` from a scroll event. */
export function updateIsAtBottom(
  state: ScrollState,
  isAtBottom: boolean
): ScrollState {
  if (state.isAtBottom === isAtBottom) return state;
  return { ...state, isAtBottom };
}

/** Clear pending intent without marking it as applied (intent went stale). */
export function clearPendingIntent(state: ScrollState): ScrollState {
  if (state.pendingIntent === null) return state;
  return { ...state, pendingIntent: null };
}

// ---------------------------------------------------------------------------
// Near-Bottom Detection
// ---------------------------------------------------------------------------

/**
 * Check whether a scroll container is within `NEAR_BOTTOM_THRESHOLD_PX`
 * of the bottom. Returns true for non-finite inputs (unmounted containers).
 */
export function isNearBottom(
  scrollTop: number,
  clientHeight: number,
  scrollHeight: number
): boolean {
  if (
    !Number.isFinite(scrollTop) ||
    !Number.isFinite(clientHeight) ||
    !Number.isFinite(scrollHeight)
  ) {
    return true;
  }

  const distanceFromBottom = scrollHeight - clientHeight - scrollTop;
  return distanceFromBottom <= NEAR_BOTTOM_THRESHOLD_PX;
}

// ---------------------------------------------------------------------------
// Bottom-Lock Release
// ---------------------------------------------------------------------------

/**
 * Minimum upward `scrollTop` delta (px) that may count as a user scroll-up.
 * Filters sub-pixel rounding and input-resize micro-adjustments.
 */
export const USER_SCROLL_UP_RELEASE_THRESHOLD_PX = 5;

export interface BottomLockReleaseInput {
  /** Whether the list is currently pinned to the bottom. */
  readonly bottomLocked: boolean;
  /** `scrollTop` recorded on the previous scroll event. */
  readonly prevScrollTop: number;
  /** `scrollTop` for the current scroll event. */
  readonly currentScrollTop: number;
  /** `scrollHeight` recorded on the previous scroll event. */
  readonly prevScrollHeight: number;
  /** `scrollHeight` for the current scroll event. */
  readonly currentScrollHeight: number;
  /** True while a programmatic (e.g. smooth) scroll we initiated is in flight. */
  readonly withinProgrammaticScroll: boolean;
  /** True while an interaction-anchor size correction is being applied. */
  readonly sizeAdjustmentActive: boolean;
}

/**
 * Decide whether a scroll event should release the bottom-lock.
 *
 * The lock means "keep the conversation pinned to the bottom as it grows." It
 * must only be released by a genuine user scroll-up — never by our own
 * programmatic scrolls, and never by content-driven `scrollTop` changes.
 *
 * The subtle case (root cause of load-time scroll oscillation): while history
 * streams in, rows are re-measured smaller than their estimates, so
 * `scrollHeight` shrinks and the browser clamps `scrollTop` downward (the
 * bottom-lock re-pin does the same). Inspecting `scrollTop` alone, that is
 * indistinguishable from a user scrolling up — so the lock would be released,
 * the next batch would re-lock, and the view would oscillate until heights
 * settle. We disambiguate by also inspecting `scrollHeight`: an upward move
 * that coincides with shrinking content is content-driven, not a user gesture,
 * so the lock stays engaged.
 */
export function shouldReleaseBottomLock(
  input: BottomLockReleaseInput
): boolean {
  const {
    bottomLocked,
    prevScrollTop,
    currentScrollTop,
    prevScrollHeight,
    currentScrollHeight,
    withinProgrammaticScroll,
    sizeAdjustmentActive,
  } = input;

  if (!bottomLocked) return false;
  if (withinProgrammaticScroll) return false;
  if (sizeAdjustmentActive) return false;

  const movedUp =
    prevScrollTop - currentScrollTop > USER_SCROLL_UP_RELEASE_THRESHOLD_PX;
  if (!movedUp) return false;

  // Content above the viewport shrinking (re-measure or browser clamp) moves
  // scrollTop up with no user input. Only a stable-height upward move is a
  // real gesture worth releasing the lock for.
  const heightShrank = currentScrollHeight < prevScrollHeight - 1;
  return !heightShrank;
}

// ---------------------------------------------------------------------------
// Intent Equality (for deduplication)
// ---------------------------------------------------------------------------

/** Structural equality check for scroll intents. */
export function intentsEqual(
  a: ScrollIntent | null,
  b: ScrollIntent | null
): boolean {
  if (a === b) return true;
  if (a === null || b === null) return false;
  if (a.type !== b.type) return false;

  switch (a.type) {
    case 'initial-bottom':
    case 'preserve-anchor':
    case 'plan-reveal':
      return true;
    case 'follow-bottom':
      return (b as FollowBottomIntent).behavior === a.behavior;
    case 'jump-to-bottom':
      return (b as JumpToBottomIntent).behavior === a.behavior;
    case 'jump-to-index': {
      const bIdx = b as JumpToIndexIntent;
      return (
        bIdx.index === a.index &&
        bIdx.align === a.align &&
        bIdx.behavior === a.behavior
      );
    }
  }
}
