import { useCallback, useContext, useMemo, type ReactNode } from "react";
import { useParams } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import type { Workspace } from "shared/types";
import {
  ActionsContext,
  type ActionsContextValue,
} from "@/shared/hooks/useActions";
import { UserContext } from "@/shared/hooks/useUserContext";
import {
  type ActionDefinition,
  type ActionExecutorContext,
  type ActionVisibilityContext,
  getActionLabel,
  resolveLabel,
} from "@/shared/types/actions";
import { SettingsDialog } from "@/shared/dialogs/settings/SettingsDialog";
import { useAppNavigation } from "@/shared/hooks/useAppNavigation";
import { useAppRuntime } from "@/shared/hooks/useAppRuntime";

interface RemoteActionsProviderProps {
  children: ReactNode;
}

function noOpSelection(name: string) {
  console.warn(`[RemoteActionsProvider] ${name} is unavailable in remote web.`);
}

export function RemoteActionsProvider({
  children,
}: RemoteActionsProviderProps) {
  const appRuntime = useAppRuntime();
  const appNavigation = useAppNavigation();
  const queryClient = useQueryClient();
  const { hostId } = useParams({ strict: false });
  const userCtx = useContext(UserContext);

  const executorContext = useMemo<ActionExecutorContext>(
    () => ({
      appRuntime,
      currentHostId: hostId ?? null,
      appNavigation,
      queryClient,
      selectWorkspace: () => {
        noOpSelection("Workspace actions");
      },
      activeWorkspaces: [],
      currentWorkspaceId: null,
      containerRef: null,
      runningDevServers: [],
      startDevServer: () => {
        noOpSelection("Dev server actions");
      },
      stopDevServer: () => {
        noOpSelection("Dev server actions");
      },
      currentLogs: null,
      logsPanelContent: null,
      remoteWorkspaces: userCtx?.workspaces ?? [],
    }),
    [appRuntime, hostId, appNavigation, queryClient, userCtx?.workspaces],
  );

  const executeAction = useCallback(
    async (action: ActionDefinition): Promise<void> => {
      if (action.id === "settings") {
        await SettingsDialog.show({
          initialSection: "organizations",
        });
        return;
      }

      console.warn(
        `[RemoteActionsProvider] Action "${action.id}" is unavailable in remote web.`,
      );
    },
    [],
  );

  const getLabel = useCallback(
    (
      action: ActionDefinition,
      workspace?: Workspace,
      ctx?: ActionVisibilityContext,
    ) => {
      if (ctx) {
        return getActionLabel(action, ctx, workspace);
      }
      return resolveLabel(action, workspace);
    },
    [],
  );

  const value = useMemo<ActionsContextValue>(
    () => ({
      executeAction,
      getLabel,
      executorContext,
    }),
    [executeAction, getLabel, executorContext],
  );

  return (
    <ActionsContext.Provider value={value}>{children}</ActionsContext.Provider>
  );
}
