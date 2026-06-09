import { describe, it, expect } from 'vitest';
import {
  shouldReleaseBottomLock,
  shouldSuppressAutoBottomIntent,
} from './conversation-scroll-commands';

describe('shouldSuppressAutoBottomIntent', () => {
  it('suppresses initial-bottom when the user is reading away from bottom', () => {
    expect(
      shouldSuppressAutoBottomIntent({
        intentType: 'initial-bottom',
        userScrollInputRecent: true,
        isAtBottom: false,
      })
    ).toBe(true);
  });

  it('suppresses follow-bottom when the user is reading away from bottom', () => {
    expect(
      shouldSuppressAutoBottomIntent({
        intentType: 'follow-bottom',
        userScrollInputRecent: true,
        isAtBottom: false,
      })
    ).toBe(true);
  });

  it('keeps the pin when the user scrolls but is still at the bottom', () => {
    expect(
      shouldSuppressAutoBottomIntent({
        intentType: 'follow-bottom',
        userScrollInputRecent: true,
        isAtBottom: true,
      })
    ).toBe(false);
  });

  it('executes auto-bottom intents when there is no recent user input', () => {
    expect(
      shouldSuppressAutoBottomIntent({
        intentType: 'initial-bottom',
        userScrollInputRecent: false,
        isAtBottom: false,
      })
    ).toBe(false);
  });

  it('never suppresses explicit or anchor-preserving intents', () => {
    for (const intentType of [
      'jump-to-bottom',
      'jump-to-index',
      'preserve-anchor',
      'plan-reveal',
    ] as const) {
      expect(
        shouldSuppressAutoBottomIntent({
          intentType,
          userScrollInputRecent: true,
          isAtBottom: false,
        })
      ).toBe(false);
    }
  });
});

describe('shouldReleaseBottomLock', () => {
  const base = {
    bottomLocked: true,
    prevScrollTop: 1000,
    currentScrollTop: 1000,
    prevScrollHeight: 5000,
    currentScrollHeight: 5000,
    withinProgrammaticScroll: false,
    sizeAdjustmentActive: false,
  };

  it('releases on a genuine stable-height user scroll-up', () => {
    expect(shouldReleaseBottomLock({ ...base, currentScrollTop: 900 })).toBe(
      true
    );
  });

  it('keeps the lock when an upward move coincides with shrinking content', () => {
    expect(
      shouldReleaseBottomLock({
        ...base,
        currentScrollTop: 900,
        currentScrollHeight: 4800,
      })
    ).toBe(false);
  });

  it('ignores sub-threshold movement', () => {
    expect(shouldReleaseBottomLock({ ...base, currentScrollTop: 997 })).toBe(
      false
    );
  });
});
