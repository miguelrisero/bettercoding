import { WorkspaceFilesPanel } from './WorkspaceFilesPanel';

interface WorkspaceFilesContainerProps {
  workspaceId: string;
}

/**
 * Thin container for the local-only Workspace Files panel. Keyed by workspace
 * id so navigation state resets when the selected workspace changes.
 */
export function WorkspaceFilesContainer({
  workspaceId,
}: WorkspaceFilesContainerProps) {
  return <WorkspaceFilesPanel key={workspaceId} workspaceId={workspaceId} />;
}
