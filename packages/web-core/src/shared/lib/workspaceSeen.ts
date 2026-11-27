export interface WorkspaceSeenState {
  workspaceId: string | undefined;
  hasUnseenActivity: boolean;
}

/**
 * Whether to mark the open workspace seen: when the user opens it, and again
 * when its agent finishes while it is still open. The server marks every
 * finished turn unseen, so marking only on open leaves the open workspace
 * reading "Finished, unread".
 */
export function shouldMarkWorkspaceSeen(
  previous: WorkspaceSeenState | null,
  current: WorkspaceSeenState & { isCreateMode: boolean }
): boolean {
  if (!current.workspaceId || current.isCreateMode) return false;
  return (
    current.workspaceId !== previous?.workspaceId || current.hasUnseenActivity
  );
}
