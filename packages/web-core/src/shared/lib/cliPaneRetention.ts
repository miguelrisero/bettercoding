/**
 * Which workspace's CLI pane stays mounted. A pane that has been shown stays
 * mounted (hidden) while chat is shown for the same workspace, so toggling
 * back reuses its socket and xterm buffer. Selecting another workspace drops
 * it; a workspace whose CLI pane was never shown mounts nothing.
 */
export function retainedCliWorkspace(
  previous: string | null,
  workspaceId: string | null,
  cliShown: boolean
): string | null {
  if (!workspaceId) return null;
  if (cliShown) return workspaceId;
  return previous === workspaceId ? previous : null;
}
