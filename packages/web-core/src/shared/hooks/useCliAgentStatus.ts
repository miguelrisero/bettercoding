import { useCallback, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type { CliAgentStatus } from 'shared/types';

import { cliAgentApi } from '@/shared/lib/api';

export interface UseCliAgentStatusResult {
  status: CliAgentStatus | undefined;
  /** Only true when the server positively established the agent is gone. */
  showRestart: boolean;
  restarting: boolean;
  restartError: string | null;
  restart: () => void;
  dismiss: () => void;
}

/**
 * Whether this workspace's CLI pane still has a live agent.
 *
 * Polling is deliberately absent. The failure this detects is a pane that is
 * *already* dead when the workspace loads — reattaching to a session whose
 * agent exited gives a dead shell no matter how often you reload — so a fetch
 * on mount and on window focus covers it. Adding an interval here would put a
 * `/proc` scan on a timer for every open workspace, which is the kind of
 * background cost this release is trying to remove.
 */
export function useCliAgentStatus(
  workspaceId: string | undefined,
  enabled = true
): UseCliAgentStatusResult {
  const queryClient = useQueryClient();
  const [dismissed, setDismissed] = useState(false);
  const [restartError, setRestartError] = useState<string | null>(null);
  const queryKey = ['cliAgentStatus', workspaceId] as const;

  const { data: status } = useQuery({
    queryKey,
    queryFn: () => cliAgentApi.status(workspaceId as string),
    enabled: Boolean(workspaceId) && enabled,
    staleTime: 10_000,
    // A probe failure is reported as `agent_alive: null`, which renders
    // nothing — so a retry storm buys us no signal.
    retry: false,
  });

  const { mutate, isPending } = useMutation({
    mutationFn: () => cliAgentApi.restart(workspaceId as string),
    onMutate: () => setRestartError(null),
    onSuccess: () => {
      setDismissed(true);
      // Resuming a large transcript can take ~10s before the agent is visible
      // in /proc, so re-probing immediately would report Absent again and put
      // the banner straight back. The pane itself is the progress indicator;
      // the next focus or remount refreshes the status.
      queryClient.invalidateQueries({ queryKey });
    },
    onError: (error: Error) =>
      setRestartError(error.message || 'Restart failed'),
  });

  const dismiss = useCallback(() => setDismissed(true), []);
  const restart = useCallback(() => {
    if (workspaceId) mutate();
  }, [mutate, workspaceId]);

  return {
    status,
    showRestart: Boolean(status?.restartable) && !dismissed,
    restarting: isPending,
    restartError,
    restart,
    dismiss,
  };
}
