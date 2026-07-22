import type { DispatchOutcome, QueueStatus } from 'shared/types';

export class DispatchConflictError extends Error {
  constructor(public readonly status: QueueStatus) {
    super('The queued message changed before this request could be accepted');
    this.name = 'DispatchConflictError';
  }
}

export class DispatchReplacementCancelledError extends Error {
  constructor() {
    super('Queued message replacement was cancelled');
    this.name = 'DispatchReplacementCancelledError';
  }
}

export async function dispatchWithConflictResolution(
  send: (replace: boolean) => Promise<DispatchOutcome>,
  confirmReplacement?: (status: QueueStatus) => Promise<boolean>
): Promise<DispatchOutcome> {
  const first = await send(false);
  if (first.outcome !== 'conflict') return first;
  if (!confirmReplacement) throw new DispatchConflictError(first.status);
  if (!(await confirmReplacement(first.status))) {
    throw new DispatchReplacementCancelledError();
  }

  const replacement = await send(true);
  if (replacement.outcome === 'conflict') {
    throw new DispatchConflictError(replacement.status);
  }
  return replacement;
}
