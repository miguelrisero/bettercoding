import { useContext } from 'react';
import { createHmrContext } from '@/shared/lib/hmrContext';
import type { Workspace } from 'shared/types';
import type {
  ActionDefinition,
  ActionExecutorContext,
  ActionVisibilityContext,
} from '@/shared/types/actions';

export interface ActionsContextValue {
  // Execute an action with optional workspaceId and repoId (for git actions)
  executeAction: (
    action: ActionDefinition,
    workspaceId?: string,
    repoId?: string
  ) => Promise<void>;

  // Get resolved label for an action (supports dynamic labels via visibility context)
  getLabel: (
    action: ActionDefinition,
    workspace?: Workspace,
    ctx?: ActionVisibilityContext
  ) => string;

  // The executor context (for components that need direct access)
  executorContext: ActionExecutorContext;
}

export const ActionsContext = createHmrContext<ActionsContextValue | null>(
  'ActionsContext',
  null
);

export function useActions(): ActionsContextValue {
  const context = useContext(ActionsContext);
  if (!context) {
    throw new Error('useActions must be used within an ActionsProvider');
  }
  return context;
}
