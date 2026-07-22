import { DispatchReplacementCancelledError } from '@/shared/lib/dispatchWithConflictResolution';

export function shouldReportResolveConflictsError(error: unknown): boolean {
  return !(error instanceof DispatchReplacementCancelledError);
}
