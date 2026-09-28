import { useQuery } from '@tanstack/react-query';
import { workspacesApi } from '@/shared/lib/api';
import { usePaneVisible } from '@/shared/hooks/PaneVisibilityContext';

export function useBranchStatus(workspaceId?: string) {
  // A hidden pane stops polling; the panel outside it keeps the status fresh.
  const paneVisible = usePaneVisible();
  return useQuery({
    queryKey: ['branchStatus', workspaceId],
    queryFn: () => workspacesApi.getBranchStatus(workspaceId!),
    enabled: !!workspaceId,
    subscribed: paneVisible,
    refetchInterval: 5000,
  });
}
