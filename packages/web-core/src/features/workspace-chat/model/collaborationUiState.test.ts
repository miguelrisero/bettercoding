import { describe, expect, it } from 'vitest';
import type {
  DispatchOutcome,
  ExecutionProcess,
  ExecutorConfig,
  QueuedMessage,
  QueueStatus,
} from 'shared/types';

import {
  deriveQueueChipState,
  mapDispatchOutcomeToUiState,
} from './collaborationUiState';

const executorConfig = { executor: 'CLAUDE_CODE' } as ExecutorConfig;

function queuedMessage(overrides: Partial<QueuedMessage> = {}): QueuedMessage {
  return {
    id: 'queue-id',
    session_id: 'session-id',
    data: {
      message: 'Keep the exact queued text',
      executor_config: executorConfig,
    },
    source: 'ui',
    state: 'queued',
    failure_reason: null,
    claude_session_id: null,
    pasted_at: null,
    acked_at: null,
    queued_at: '2026-07-21T10:00:00.000Z',
    updated_at: '2026-07-21T10:00:00.000Z',
    ...overrides,
  };
}

function queueStatus(
  status: 'queued' | 'pasting' | 'pasted',
  message = queuedMessage({ state: status })
): QueueStatus {
  return { status, message } as QueueStatus;
}

describe('mapDispatchOutcomeToUiState', () => {
  it('maps a started outcome to normal composer cleanup without a notice', () => {
    const outcome: DispatchOutcome = {
      outcome: 'started',
      execution_process: {} as ExecutionProcess,
    };

    expect(mapDispatchOutcomeToUiState(outcome)).toEqual({
      kind: 'started',
      notice: 'none',
      queueStatus: { status: 'empty' },
    });
  });

  it('maps every non-started outcome to visible queue state or replacement confirmation', () => {
    const queued = queueStatus('queued');
    const pasted = queueStatus('pasted');

    expect(
      mapDispatchOutcomeToUiState({ outcome: 'queued', status: queued })
    ).toMatchObject({
      kind: 'queued',
      notice: 'held',
      queueStatus: queued,
    });
    expect(
      mapDispatchOutcomeToUiState({
        outcome: 'routed_to_cli',
        delivery: pasted,
      })
    ).toMatchObject({
      kind: 'routed-to-cli',
      notice: 'routed-to-cli',
      queueStatus: pasted,
    });
    expect(
      mapDispatchOutcomeToUiState({ outcome: 'conflict', status: queued })
    ).toMatchObject({
      kind: 'conflict',
      notice: 'none',
      queueStatus: queued,
    });
  });

  it('marks a queued delivery failure for an explicit notice', () => {
    const status = queueStatus(
      'queued',
      queuedMessage({ failure_reason: 'CLI paste failed; queued for retry' })
    );

    expect(
      mapDispatchOutcomeToUiState({ outcome: 'queued', status }).notice
    ).toBe('delivery-failed');
  });

  it('distinguishes the started, queued, and routed outcomes used by fork recovery', () => {
    const started: DispatchOutcome = {
      outcome: 'started',
      execution_process: {} as ExecutionProcess,
    };
    const queued = queueStatus('queued');
    const pasted = queueStatus('pasted');

    expect(mapDispatchOutcomeToUiState(started).kind).toBe('started');
    expect(
      mapDispatchOutcomeToUiState({ outcome: 'queued', status: queued }).kind
    ).toBe('queued');
    expect(
      mapDispatchOutcomeToUiState({
        outcome: 'routed_to_cli',
        delivery: pasted,
      }).kind
    ).toBe('routed-to-cli');
  });
});

describe('deriveQueueChipState', () => {
  it('derives queued, pasting, and pasted persistent chip states', () => {
    expect(deriveQueueChipState(queueStatus('queued'))?.kind).toBe('queued');
    expect(deriveQueueChipState(queueStatus('pasting'))?.kind).toBe('pasting');
    expect(deriveQueueChipState(queueStatus('pasted'))?.kind).toBe('pasted');
    expect(deriveQueueChipState({ status: 'empty' })).toBeNull();
  });

  it('turns failure_reason into a retryable notice with Send again eligibility', () => {
    const message = queuedMessage({
      source: 'recovery',
      failure_reason: 'executor start failed; queued for retry',
    });

    expect(deriveQueueChipState(queueStatus('queued', message))).toEqual({
      kind: 'failed',
      message,
      failureReason: 'executor start failed; queued for retry',
      canSendAgain: true,
    });
  });

  it('does not offer Send again while a failed delivery is already in flight', () => {
    const chip = deriveQueueChipState(
      queueStatus(
        'pasting',
        queuedMessage({
          state: 'pasting',
          failure_reason: 'Previous delivery failed',
        })
      )
    );

    expect(chip?.kind).toBe('failed');
    expect(chip?.canSendAgain).toBe(false);
  });
});
