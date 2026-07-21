import { describe, expect, it, vi } from 'vitest';
import type { DispatchOutcome, QueueStatus } from 'shared/types';

import {
  DispatchConflictError,
  DispatchReplacementCancelledError,
  dispatchWithConflictResolution,
} from './dispatchWithConflictResolution';

const conflictStatus = {
  status: 'queued',
  message: { data: { message: 'existing queued prompt' } },
} as QueueStatus;

describe('dispatchWithConflictResolution', () => {
  it('does not report success when replacement is declined', async () => {
    const send = vi
      .fn<(_: boolean) => Promise<DispatchOutcome>>()
      .mockResolvedValue({
        outcome: 'conflict',
        status: conflictStatus,
      });

    await expect(
      dispatchWithConflictResolution(send, async () => false)
    ).rejects.toBeInstanceOf(DispatchReplacementCancelledError);
    expect(send).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenCalledWith(false);
  });

  it('retries only after explicit replacement confirmation', async () => {
    const accepted = {
      outcome: 'queued',
      status: conflictStatus,
    } as DispatchOutcome;
    const send = vi
      .fn<(_: boolean) => Promise<DispatchOutcome>>()
      .mockResolvedValueOnce({ outcome: 'conflict', status: conflictStatus })
      .mockResolvedValueOnce(accepted);

    await expect(
      dispatchWithConflictResolution(send, async () => true)
    ).resolves.toBe(accepted);
    expect(send.mock.calls).toEqual([[false], [true]]);
  });

  it('surfaces a second conflict instead of resolving successfully', async () => {
    const send = vi
      .fn<(_: boolean) => Promise<DispatchOutcome>>()
      .mockResolvedValue({
        outcome: 'conflict',
        status: conflictStatus,
      });

    await expect(
      dispatchWithConflictResolution(send, async () => true)
    ).rejects.toBeInstanceOf(DispatchConflictError);
  });
});
