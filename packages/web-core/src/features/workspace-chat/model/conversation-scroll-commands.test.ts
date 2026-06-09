import { describe, it, expect } from 'vitest';
import {
  shouldReleaseBottomLock,
  shouldSuppressAutoBottomIntent,
} from './conversation-scroll-commands';

describe('shouldSuppressAutoBottomIntent', () => {
  it('suppresses initial-bottom while the user is reading away from bottom', () => {
    expect(
      shouldSuppressAutoBottomIntent({
        intentType: 'initial-bottom',
        userScrolledAway: true,
      })
    ).toBe(true);
  });

  it('suppresses follow-bottom while the user is reading away from bottom', () => {
    expect(
      shouldSuppressAutoBottomIntent({
        intentType: 'follow-bottom',
        userScrolledAway: true,
      })
    ).toBe(true);
  });

  it('executes auto-bottom intents once the user is back at the bottom', () => {
    expect(
      shouldSuppressAutoBottomIntent({
        intentType: 'follow-bottom',
        userScrolledAway: false,
      })
    ).toBe(false);
    expect(
      shouldSuppressAutoBottomIntent({
        intentType: 'initial-bottom',
        userScrolledAway: false,
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
          userScrolledAway: true,
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
