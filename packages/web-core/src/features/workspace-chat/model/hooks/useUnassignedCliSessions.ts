import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type {
  AssignNativeCliSessionRequest,
  UnassignedCliSession,
} from 'shared/types';

import { handleApiResponse } from '@/shared/lib/api';
import { makeLocalApiRequest } from '@/shared/lib/localApiTransport';
import { useHostId } from '@/shared/providers/HostIdProvider';

const unassignedCliSessionKeys = {
  byWorkspace: (workspaceId: string | undefined, hostId: string | null) =>
    ['native-cli-sessions', 'unassigned', hostId, workspaceId] as const,
};

async function fetchUnassignedCliSessions(
  workspaceId: string
): Promise<UnassignedCliSession[]> {
  const response = await makeLocalApiRequest(
    `/api/workspaces/${workspaceId}/native-cli-sessions/unassigned`
  );
  return handleApiResponse<UnassignedCliSession[]>(response);
}

async function assignCliSession(
  request: AssignNativeCliSessionRequest
): Promise<void> {
  const response = await makeLocalApiRequest(
    '/api/native-cli-sessions/assign',
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(request),
    }
  );
  await handleApiResponse<void>(response);
}

export function useUnassignedCliSessions(
  workspaceId: string | undefined,
  sessionId: string | undefined
) {
  const hostId = useHostId();
  const queryClient = useQueryClient();
  const queryKey = unassignedCliSessionKeys.byWorkspace(workspaceId, hostId);
  const query = useQuery({
    queryKey,
    queryFn: () => fetchUnassignedCliSessions(workspaceId!),
    enabled: Boolean(workspaceId && sessionId),
    refetchInterval: 15_000,
    refetchOnWindowFocus: true,
    retry: false,
  });
  const assignment = useMutation({
    mutationFn: (claudeSessionId: string) => {
      if (!sessionId) throw new Error('No conversation selected');
      return assignCliSession({
        claude_session_id: claudeSessionId,
        session_id: sessionId,
      });
    },
    onSuccess: (_data, claudeSessionId) => {
      queryClient.setQueryData<UnassignedCliSession[]>(
        queryKey,
        (sessions = []) =>
          sessions.filter(
            (session) => session.claude_session_id !== claudeSessionId
          )
      );
      void queryClient.invalidateQueries({ queryKey });
    },
  });

  return {
    sessions: query.data ?? [],
    isLoading: query.isLoading,
    error: query.error ?? assignment.error,
    assign: assignment.mutateAsync,
    assigningSessionId: assignment.isPending ? assignment.variables : undefined,
  };
}
