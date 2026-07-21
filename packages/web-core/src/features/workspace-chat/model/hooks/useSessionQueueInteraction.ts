import { useCallback } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ApiError, queueApi } from '@/shared/lib/api';
import type { ExecutorConfig, QueuedMessage, QueueStatus } from 'shared/types';

interface UseSessionQueueInteractionOptions {
  /** Session ID for queue operations */
  sessionId: string | undefined;
}

export type QueueMessageResult =
  | { outcome: 'stored'; status: QueueStatus }
  | { outcome: 'conflict'; status: QueueStatus };

interface UseSessionQueueInteractionResult {
  /** The complete durable queue state, including CLI delivery phases. */
  queueStatus: QueueStatus;
  /** Whether the active slot can still be edited or cancelled. */
  isQueued: boolean;
  /** The active queue record in any delivery phase. */
  activeMessage: QueuedMessage | null;
  /** The editable queued message content, if any. */
  queuedMessage: string | null;
  /** The executor config from the editable queued message, if any. */
  queuedConfig: ExecutorConfig | null;
  /** Whether a queue operation is in progress. */
  isQueueLoading: boolean;
  /** Queue a message for later execution. */
  queueMessage: (
    message: string,
    executorConfig: ExecutorConfig,
    replace?: boolean
  ) => Promise<QueueMessageResult | null>;
  /** Re-enqueue the current failed slot with explicit replacement. */
  retryQueuedMessage: () => Promise<QueueMessageResult | null>;
  /** Cancel the queued message. */
  cancelQueue: () => Promise<void>;
  /** Update cached queue truth immediately from a dispatch response. */
  setQueueStatus: (status: QueueStatus) => void;
  /** Refresh queue status from server. */
  refreshQueueStatus: () => Promise<void>;
}

const QUEUE_STATUS_KEY = 'queue-status';

export const sessionQueueKeys = {
  status: (sessionId: string | undefined) =>
    [QUEUE_STATUS_KEY, sessionId] as const,
};

function queueResultFromError(error: unknown): QueueMessageResult | null {
  if (!(error instanceof ApiError)) return null;
  const queueError = error as ApiError<QueueStatus>;
  if (queueError.statusCode !== 409 || !queueError.error_data) return null;
  return { outcome: 'conflict', status: queueError.error_data };
}

/**
 * Manage the session's durable single-slot queue. Active deliveries poll once
 * per second so pasted -> imported/empty transitions clear the composer truth
 * state promptly; empty sessions poll more lightly to notice another surface.
 */
export function useSessionQueueInteraction({
  sessionId,
}: UseSessionQueueInteractionOptions): UseSessionQueueInteractionResult {
  const queryClient = useQueryClient();
  const queryKey = sessionQueueKeys.status(sessionId);

  const { data: queueStatus = { status: 'empty' as const }, refetch } =
    useQuery<QueueStatus>({
      queryKey,
      queryFn: () => queueApi.getStatus(sessionId!),
      enabled: Boolean(sessionId),
      refetchInterval: (query) => {
        const status = query.state.data;
        return status && status.status !== 'empty' ? 1_000 : 5_000;
      },
    });

  const activeMessage =
    queueStatus.status === 'empty' ? null : queueStatus.message;
  const editableMessage =
    queueStatus.status === 'queued' ? queueStatus.message : null;
  const isQueued = editableMessage !== null;
  const queuedMessage = editableMessage?.data.message ?? null;
  const queuedConfig = editableMessage?.data.executor_config ?? null;

  const setQueueStatus = useCallback(
    (status: QueueStatus) => {
      queryClient.setQueryData(queryKey, status);
    },
    [queryClient, queryKey]
  );

  const queueMutation = useMutation({
    mutationFn: async ({
      message,
      executorConfig,
      replace,
    }: {
      message: string;
      executorConfig: ExecutorConfig;
      replace: boolean;
    }): Promise<QueueMessageResult> => {
      try {
        const status = await queueApi.queue(sessionId!, {
          message,
          executor_config: executorConfig,
          replace,
        });
        return { outcome: 'stored', status };
      } catch (error) {
        const conflict = queueResultFromError(error);
        if (conflict) return conflict;
        throw error;
      }
    },
    onSuccess: (result) => {
      setQueueStatus(result.status);
    },
  });

  const cancelMutation = useMutation({
    mutationFn: () => queueApi.cancel(sessionId!),
    onSuccess: setQueueStatus,
  });

  const queueMessage = useCallback(
    async (
      message: string,
      executorConfig: ExecutorConfig,
      replace = false
    ) => {
      if (!sessionId) return null;
      return queueMutation.mutateAsync({
        message,
        executorConfig,
        replace,
      });
    },
    [sessionId, queueMutation]
  );

  const retryQueuedMessage = useCallback(async () => {
    if (!editableMessage) return null;
    return queueMutation.mutateAsync({
      message: editableMessage.data.message,
      executorConfig: editableMessage.data.executor_config,
      replace: true,
    });
  }, [editableMessage, queueMutation]);

  const cancelQueue = useCallback(async () => {
    if (!sessionId) return;
    await cancelMutation.mutateAsync();
  }, [sessionId, cancelMutation]);

  const refreshQueueStatus = useCallback(async () => {
    if (!sessionId) return;
    await refetch();
  }, [sessionId, refetch]);

  return {
    queueStatus,
    isQueued,
    activeMessage,
    queuedMessage,
    queuedConfig,
    isQueueLoading: queueMutation.isPending || cancelMutation.isPending,
    queueMessage,
    retryQueuedMessage,
    cancelQueue,
    setQueueStatus,
    refreshQueueStatus,
  };
}
