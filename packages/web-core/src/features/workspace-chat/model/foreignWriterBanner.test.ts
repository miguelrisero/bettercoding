import { describe, expect, it } from 'vitest';

import { shouldShowForeignWriterBanner } from './foreignWriterBanner';

describe('shouldShowForeignWriterBanner', () => {
  const firstSeenAt = '2026-07-21T10:00:00.000Z';

  it('shows a newly detected foreign writer', () => {
    expect(shouldShowForeignWriterBanner(firstSeenAt, null)).toBe(true);
  });

  it('keeps the dismissed timestamp hidden', () => {
    expect(shouldShowForeignWriterBanner(firstSeenAt, firstSeenAt)).toBe(false);
  });

  it('re-raises the banner only for a newer foreign_writer_seen_at', () => {
    expect(
      shouldShowForeignWriterBanner('2026-07-21T10:01:00.000Z', firstSeenAt)
    ).toBe(true);
    expect(
      shouldShowForeignWriterBanner('2026-07-21T09:59:00.000Z', firstSeenAt)
    ).toBe(false);
  });

  it('stays hidden when detection has not reported a writer', () => {
    expect(shouldShowForeignWriterBanner(null, firstSeenAt)).toBe(false);
  });
});
