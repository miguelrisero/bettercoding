/**
 * Which workspace's pane (CLI or chat) stays mounted. A pane that has been
 * shown stays mounted (hidden) while the other pane is shown for the same
 * workspace, so toggling back reuses its live state. Selecting another
 * workspace drops it; a workspace whose pane was never shown mounts nothing.
 */
export function retainedPaneWorkspace(
  previous: string | null,
  workspaceId: string | null,
  shown: boolean
): string | null {
  if (!workspaceId) return null;
  if (shown) return workspaceId;
  return previous === workspaceId ? previous : null;
}
