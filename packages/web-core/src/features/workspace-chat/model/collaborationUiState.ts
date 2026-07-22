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
}

export type QueueChipKind = 'queued' | 'pasting' | 'pasted' | 'failed';

export interface QueueChipState {
  kind: QueueChipKind;
  message: QueuedMessage;
  failureReason: string | null;
  canSendAgain: boolean;
}

export function assertNever(value: never): never {
  throw new Error(`Unhandled collaboration state: ${String(value)}`);
}

export function getActiveQueuedMessage(
  status: QueueStatus
): QueuedMessage | null {
  switch (status.status) {
    case 'empty':
      return null;
    case 'queued':
    case 'pasting':
    case 'pasted':
      return status.message;
    default:
      return assertNever(status);
  }
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
      };
    case 'queued': {
      const message = getActiveQueuedMessage(outcome.status);
      return {
        kind: 'queued',
        notice: message?.failure_reason ? 'delivery-failed' : 'held',
        queueStatus: outcome.status,
      };
    }
    case 'routed_to_cli':
      return {
        kind: 'routed-to-cli',
        notice: 'routed-to-cli',
        queueStatus: outcome.delivery,
      };
    case 'conflict':
      return {
        kind: 'conflict',
        notice: 'none',
        queueStatus: outcome.status,
      };
    default:
      return assertNever(outcome);
  }
}

/** Derive the persistent composer chip from the durable queue slot. */
export function deriveQueueChipState(
  status: QueueStatus
): QueueChipState | null {
  switch (status.status) {
    case 'empty':
      return null;
    case 'queued':
    case 'pasting':
    case 'pasted': {
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
    default:
      return assertNever(status);
  }
}
