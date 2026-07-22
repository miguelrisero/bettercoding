import { describe, expect, it } from 'vitest';

import { DispatchReplacementCancelledError } from '@/shared/lib/dispatchWithConflictResolution';
import { shouldReportResolveConflictsError } from './resolveConflictsError';

describe('shouldReportResolveConflictsError', () => {
  it('treats an explicitly cancelled queue replacement as a no-op', () => {
    expect(
      shouldReportResolveConflictsError(new DispatchReplacementCancelledError())
    ).toBe(false);
  });

  it('still reports genuine conflict-resolution failures', () => {
    expect(
      shouldReportResolveConflictsError(new Error('backend unavailable'))
    ).toBe(true);
  });
});
