import { useEffect, useRef, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { workspacesApi } from '@/shared/lib/api';
import { workspaceSummaryKeys } from '@/shared/hooks/workspaceSummaryKeys';
import {
  shouldMarkWorkspaceSeen,
  type WorkspaceSeenState,
} from '@/shared/lib/workspaceSeen';

function isPageVisible() {
  return document.visibilityState === 'visible';
}

/** Whether the user can see the page, updated on `visibilitychange`. */
function usePageVisible() {
  const [visible, setVisible] = useState(isPageVisible);
  useEffect(() => {
    const update = () => setVisible(isPageVisible());
    document.addEventListener('visibilitychange', update);
    return () => document.removeEventListener('visibilitychange', update);
  }, []);
  return visible;
}

/**
 * Mark the open workspace seen on open and when its agent finishes. A
 * background tab leaves it unread until the user returns to the page.
 */
export function useMarkWorkspaceSeen(
  workspaceId: string | undefined,
  isCreateMode: boolean,
  hasUnseenActivity: boolean
) {
  const queryClient = useQueryClient();
  const isVisible = usePageVisible();
  const lastSeenStateRef = useRef<WorkspaceSeenState | null>(null);
  useEffect(() => {
    // Hidden: record nothing, so the return to the page decides.
    if (!isVisible) return;
    const previous = lastSeenStateRef.current;
    lastSeenStateRef.current = {
      workspaceId: isCreateMode ? undefined : workspaceId,
      hasUnseenActivity,
    };
    if (
      !workspaceId ||
      !shouldMarkWorkspaceSeen(previous, {
        workspaceId,
        isCreateMode,
        hasUnseenActivity,
      })
    ) {
      return;
    }

    workspacesApi
      .markSeen(workspaceId)
      .then(() => {
        queryClient.invalidateQueries({ queryKey: workspaceSummaryKeys.all });
      })
      .catch((error) => {
        console.warn('Failed to mark workspace as seen:', error);
      });
  }, [workspaceId, isCreateMode, hasUnseenActivity, isVisible, queryClient]);
}
