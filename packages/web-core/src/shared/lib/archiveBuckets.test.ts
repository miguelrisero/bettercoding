import { describe, expect, it } from 'vitest';
import {
  DAY_IN_MILLISECONDS,
  archiveBucketForTimestamp,
  archiveBucketFromAgeMilliseconds,
  isArchivedRecently,
} from './archiveBuckets';

const millisecond = 1;

describe('archiveBucketFromAgeMilliseconds', () => {
  it.each([
    [0, 'today'],
    [millisecond, 'today'],
    [DAY_IN_MILLISECONDS - millisecond, 'today'],
    [DAY_IN_MILLISECONDS, 'one_to_three_days'],
    [DAY_IN_MILLISECONDS + millisecond, 'one_to_three_days'],
    [3 * DAY_IN_MILLISECONDS - millisecond, 'one_to_three_days'],
    [3 * DAY_IN_MILLISECONDS, 'three_to_seven_days'],
    [3 * DAY_IN_MILLISECONDS + millisecond, 'three_to_seven_days'],
    [7 * DAY_IN_MILLISECONDS - millisecond, 'three_to_seven_days'],
    [7 * DAY_IN_MILLISECONDS, 'seven_to_fifteen_days'],
    [7 * DAY_IN_MILLISECONDS + millisecond, 'seven_to_fifteen_days'],
    [15 * DAY_IN_MILLISECONDS - millisecond, 'seven_to_fifteen_days'],
    [15 * DAY_IN_MILLISECONDS, 'fifteen_to_thirty_days'],
    [15 * DAY_IN_MILLISECONDS + millisecond, 'fifteen_to_thirty_days'],
    [30 * DAY_IN_MILLISECONDS - millisecond, 'fifteen_to_thirty_days'],
    [30 * DAY_IN_MILLISECONDS, 'older_than_thirty_days'],
    [30 * DAY_IN_MILLISECONDS + millisecond, 'older_than_thirty_days'],
  ] as const)('maps age %i ms to %s', (age, expected) => {
    expect(archiveBucketFromAgeMilliseconds(age)).toBe(expected);
  });

  it('clamps future timestamps to today', () => {
    expect(archiveBucketFromAgeMilliseconds(-10 * DAY_IN_MILLISECONDS)).toBe(
      'today'
    );
  });
});

describe('archiveBucketForTimestamp', () => {
  const now = Date.UTC(2026, 6, 22, 12);

  it('puts missing or invalid timestamps in the defensive oldest bucket', () => {
    expect(archiveBucketForTimestamp(null, now)).toBe('older_than_thirty_days');
    expect(archiveBucketForTimestamp('not-a-date', now)).toBe(
      'older_than_thirty_days'
    );
  });

  it('uses the Today plus 1–3 day buckets for Archived recently', () => {
    expect(
      isArchivedRecently(
        new Date(now - 3 * DAY_IN_MILLISECONDS + millisecond).toISOString(),
        now
      )
    ).toBe(true);
    expect(
      isArchivedRecently(
        new Date(now - 3 * DAY_IN_MILLISECONDS).toISOString(),
        now
      )
    ).toBe(false);
  });
});
