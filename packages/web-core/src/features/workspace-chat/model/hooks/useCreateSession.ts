import { useMutation, useQueryClient } from '@tanstack/react-query';
import { sessionsApi } from '@/shared/lib/api';
import { useHostId } from '@/shared/providers/HostIdProvider';
import { workspaceSessionKeys } from '@/shared/hooks/workspaceSessionKeys';
import type {
  Session,
  CreateFollowUpAttempt,
  DispatchOutcome,
  ExecutorConfig,
} from 'shared/types';

interface CreateSessionParams {
  workspaceId: string;
  prompt: string;
  executorConfig: ExecutorConfig;
}

interface CreatedSessionDispatch {
  session: Session;
  outcome: DispatchOutcome;
}

/**
 * Hook for creating a new session and sending the first message.
 * Uses TanStack Query mutation for proper cache management.
 */
export function useCreateSession() {
  const queryClient = useQueryClient();
  const hostId = useHostId();

  return useMutation({
    mutationFn: async ({
      workspaceId,
      prompt,
      executorConfig,
    }: CreateSessionParams): Promise<CreatedSessionDispatch> => {
      const session = await sessionsApi.create({
        workspace_id: workspaceId,
      });

      const body: CreateFollowUpAttempt = {
        prompt,
        executor_config: executorConfig,
        retry_process_id: null,
        force_when_dirty: null,
        perform_git_reset: null,
        replace: false,
      };
      const outcome = await sessionsApi.followUp(session.id, body);

      return { session, outcome };
    },
    onSuccess: ({ session }) => {
      // Invalidate session queries to refresh the list
      queryClient.invalidateQueries({
        queryKey: workspaceSessionKeys.byWorkspace(
          session.workspace_id,
          hostId
        ),
      });
    },
  });
}
