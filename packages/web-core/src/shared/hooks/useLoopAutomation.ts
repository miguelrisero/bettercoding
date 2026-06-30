import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { loopAutomationApi } from '@/shared/lib/api';
import type {
  CreateWakeupRequest,
  LoopAutomationStatus,
  UpsertLoopAutomationRequest,
} from 'shared/types';

export const loopAutomationKeys = {
  all: ['loop-automation'] as const,
  byWorkspace: (workspaceId: string) =>
    ['loop-automation', workspaceId] as const,
};

/**
 * Inputs for the "Keep going" policy expressed with plain numbers — the
 * generated request type stores the count fields as `bigint`, but JSON only
 * serialises numbers, so we coerce here (matching the repo's existing
 * `as unknown as bigint` convention).
 */
export interface UpdateLoopPolicyInput {
  enabled: boolean;
  retryIntervalSecs?: number | null;
  continuationPrompt?: string | null;
  maxAttempts?: number | null;
}

const toBigint = (value: number | null | undefined): bigint | null =>
  value == null ? null : (value as unknown as bigint);

/**
 * Status query + mutations for a workspace's loop-automation ("Keep going")
 * policy. The PUT endpoint returns the full, freshly-resolved status, so we
 * seed the cache from its response; the wakeup mutations invalidate to refetch.
 */
export function useLoopAutomation(workspaceId: string | undefined) {
  const queryClient = useQueryClient();

  const statusQuery = useQuery<LoopAutomationStatus>({
    queryKey: loopAutomationKeys.byWorkspace(workspaceId ?? ''),
    queryFn: () => loopAutomationApi.getStatus(workspaceId!),
    enabled: !!workspaceId,
  });

  const invalidate = () => {
    if (!workspaceId) return;
    queryClient.invalidateQueries({
      queryKey: loopAutomationKeys.byWorkspace(workspaceId),
    });
  };

  const updatePolicy = useMutation({
    mutationFn: (input: UpdateLoopPolicyInput) => {
      const body: UpsertLoopAutomationRequest = {
        enabled: input.enabled,
        retry_interval_secs: toBigint(input.retryIntervalSecs),
        continuation_prompt: input.continuationPrompt ?? null,
        max_attempts: toBigint(input.maxAttempts),
      };
      return loopAutomationApi.updatePolicy(workspaceId!, body);
    },
    onSuccess: (status) => {
      if (!workspaceId) return;
      queryClient.setQueryData(
        loopAutomationKeys.byWorkspace(workspaceId),
        status
      );
    },
    onError: (err) => {
      console.error('Failed to update loop automation:', err);
    },
  });

  const addWakeup = useMutation({
    mutationFn: (body: CreateWakeupRequest) =>
      loopAutomationApi.createWakeup(workspaceId!, body),
    onSuccess: invalidate,
    onError: (err) => {
      console.error('Failed to schedule wakeup:', err);
    },
  });

  const deleteWakeup = useMutation({
    mutationFn: (wakeupId: string) => loopAutomationApi.deleteWakeup(wakeupId),
    onSuccess: invalidate,
    onError: (err) => {
      console.error('Failed to cancel wakeup:', err);
    },
  });

  return {
    status: statusQuery.data,
    isLoading: statusQuery.isLoading,
    updatePolicy,
    addWakeup,
    deleteWakeup,
  };
}
