import type { DispatchOutcome, QueuedMessage, QueueStatus } from 'shared/types';

export type DispatchUiKind =
  | 'started'
  | 'queued'
  | 'routed-to-cli'
  | 'conflict';

export type DispatchNoticeKind =
  | 'none'
  | 'held'
  | 'delivery-failed'
  | 'routed-to-cli';

export interface DispatchUiState {
  kind: DispatchUiKind;
  notice: DispatchNoticeKind;
  queueStatus: QueueStatus;
  clearComposer: boolean;
  confirmReplacement: boolean;
}

export type QueueChipKind = 'queued' | 'pasting' | 'pasted' | 'failed';

export interface QueueChipState {
  kind: QueueChipKind;
  message: QueuedMessage;
  failureReason: string | null;
  canSendAgain: boolean;
}

export function getActiveQueuedMessage(
  status: QueueStatus
): QueuedMessage | null {
  return status.status === 'empty' ? null : status.message;
}

/**
 * Convert the backend's dispatch decision into explicit composer behavior.
 * Keeping this pure makes it impossible for a new outcome to be silently
 * treated as a successful executor start.
 */
export function mapDispatchOutcomeToUiState(
  outcome: DispatchOutcome
): DispatchUiState {
  switch (outcome.outcome) {
    case 'started':
      return {
        kind: 'started',
        notice: 'none',
        queueStatus: { status: 'empty' },
        clearComposer: true,
        confirmReplacement: false,
      };
    case 'queued': {
      const message = getActiveQueuedMessage(outcome.status);
      return {
        kind: 'queued',
        notice: message?.failure_reason ? 'delivery-failed' : 'held',
        queueStatus: outcome.status,
        clearComposer: true,
        confirmReplacement: false,
      };
    }
    case 'routed_to_cli':
      return {
        kind: 'routed-to-cli',
        notice: 'routed-to-cli',
        queueStatus: outcome.delivery,
        clearComposer: true,
        confirmReplacement: false,
      };
    case 'conflict':
      return {
        kind: 'conflict',
        notice: 'none',
        queueStatus: outcome.status,
        clearComposer: false,
        confirmReplacement: true,
      };
  }
}

/** Derive the persistent composer chip from the durable queue slot. */
export function deriveQueueChipState(
  status: QueueStatus
): QueueChipState | null {
  if (status.status === 'empty') return null;

  const failureReason = status.message.failure_reason;
  if (failureReason) {
    return {
      kind: 'failed',
      message: status.message,
      failureReason,
      canSendAgain: status.status === 'queued',
    };
  }

  return {
    kind: status.status,
    message: status.message,
    failureReason: null,
    canSendAgain: false,
  };
}
