import { useContext, useCallback, useMemo, type ReactNode } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import type { Workspace } from 'shared/types';
import { ConfirmDialog } from '@vibe/ui/components/ConfirmDialog';
import { useHostId } from '@/shared/providers/HostIdProvider';
import {
  type ActionDefinition,
  type ActionExecutorContext,
  type ActionVisibilityContext,
  ActionTargetType,
  resolveLabel,
  getActionLabel,
} from '@/shared/types/actions';
import { useWorkspaceContext } from '@/shared/hooks/useWorkspaceContext';
import { UserContext } from '@/shared/hooks/useUserContext';
import { useDevServer } from '@/shared/hooks/useDevServer';
import { useLogsPanel } from '@/shared/hooks/useLogsPanel';
import { useLogStream } from '@/shared/hooks/useLogStream';
import { ActionsContext } from '@/shared/hooks/useActions';
import { useAppNavigation } from '@/shared/hooks/useAppNavigation';
import { useAppRuntime } from '@/shared/hooks/useAppRuntime';

interface ActionsProviderProps {
  children: ReactNode;
}

export function ActionsProvider({ children }: ActionsProviderProps) {
  const appRuntime = useAppRuntime();
  const appNavigation = useAppNavigation();
  const hostId = useHostId();
  const queryClient = useQueryClient();
  // Get workspace context (ActionsProvider is nested inside WorkspaceProvider)
  const { selectWorkspace, activeWorkspaces, workspaceId, workspace } =
    useWorkspaceContext();
  // Get remote workspaces (optional — not available on all routes)
  const userCtx = useContext(UserContext);
  // Get dev server state
  const { start, stop, runningDevServers } = useDevServer(workspaceId);

  // Get logs panel state
  const { logsPanelContent } = useLogsPanel();
  const processId =
    logsPanelContent?.type === 'process' ? logsPanelContent.processId : '';
  const { logs: processLogs } = useLogStream(processId);

  // Compute currentLogs based on content type
  const currentLogs = useMemo(() => {
    if (logsPanelContent?.type === 'tool') {
      return logsPanelContent.content
        .split('\n')
        .map((line) => ({ type: 'STDOUT' as const, content: line }));
    }
    if (logsPanelContent?.type === 'process') {
      return processLogs;
    }
    return null;
  }, [logsPanelContent, processLogs]);

  // Build executor context from hooks
  const executorContext = useMemo<ActionExecutorContext>(() => {
    return {
      appRuntime,
      currentHostId: hostId,
      appNavigation,
      queryClient,
      selectWorkspace,
      activeWorkspaces,
      currentWorkspaceId: workspaceId ?? null,
      containerRef: workspace?.container_ref ?? null,
      runningDevServers,
      startDevServer: start,
      stopDevServer: stop,
      currentLogs,
      logsPanelContent,
      remoteWorkspaces: userCtx?.workspaces ?? [],
    };
  }, [
    appRuntime,
    hostId,
    appNavigation,
    queryClient,
    selectWorkspace,
    activeWorkspaces,
    workspaceId,
    workspace?.container_ref,
    runningDevServers,
    start,
    stop,
    currentLogs,
    logsPanelContent,
    userCtx?.workspaces,
  ]);

  // Main action executor with centralized target validation and error handling
  const executeAction = useCallback(
    async (
      action: ActionDefinition,
      workspaceId?: string,
      repoId?: string
    ): Promise<void> => {
      try {
        switch (action.requiresTarget) {
          case ActionTargetType.NONE:
            await action.execute(executorContext);
            break;

          case ActionTargetType.WORKSPACE:
            if (!workspaceId) {
              throw new Error(
                `Action "${action.id}" requires a workspace target`
              );
            }
            await action.execute(executorContext, workspaceId);
            break;

          case ActionTargetType.GIT:
            if (!workspaceId || !repoId) {
              throw new Error(
                `Action "${action.id}" requires both workspace and repository`
              );
            }
            await action.execute(executorContext, workspaceId, repoId);
            break;
        }
      } catch (error) {
        // Show error to user via alert dialog
        ConfirmDialog.show({
          title: 'Error',
          message: error instanceof Error ? error.message : 'An error occurred',
          confirmText: 'OK',
          showCancelButton: false,
          variant: 'destructive',
        });
      }
    },
    [executorContext]
  );

  // Get resolved label helper (supports dynamic labels via visibility context)
  const getLabel = useCallback(
    (
      action: ActionDefinition,
      workspace?: Workspace,
      ctx?: ActionVisibilityContext
    ) => {
      if (ctx) {
        return getActionLabel(action, ctx, workspace);
      }
      return resolveLabel(action, workspace);
    },
    []
  );

  const value = useMemo(
    () => ({
      executeAction,
      getLabel,
      executorContext,
    }),
    [executeAction, getLabel, executorContext]
  );

  return (
    <ActionsContext.Provider value={value}>{children}</ActionsContext.Provider>
  );
}
