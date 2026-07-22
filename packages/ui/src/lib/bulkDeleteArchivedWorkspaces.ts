import type { BulkDeleteTarget } from 'shared/types';

export interface ArchivedWorkspaceBulkDeleteSource {
  id: string;
  name: string;
  archivedAt: string | null;
  repoCount?: number;
}

export interface BulkDeleteArchivedWorkspaceDetails {
  workspaceName: string;
  repoCount?: number;
}

export function buildArchivedBucketState<
  TWorkspace extends ArchivedWorkspaceBulkDeleteSource,
>(
  groupWorkspaces: readonly TWorkspace[],
  visibleWorkspaceIds?: ReadonlySet<string>
): {
  targets: BulkDeleteTarget[];
  detailsByWorkspaceId: Readonly<
    Record<string, BulkDeleteArchivedWorkspaceDetails>
  >;
  visibleWorkspaces: TWorkspace[];
} {
  return {
    targets: groupWorkspaces.map((workspace) => ({
      workspace_id: workspace.id,
      archived_at: workspace.archivedAt,
    })),
    detailsByWorkspaceId: Object.fromEntries(
      groupWorkspaces.map((workspace) => [
        workspace.id,
        {
          workspaceName: workspace.name,
          repoCount: workspace.repoCount,
        },
      ])
    ),
    visibleWorkspaces: visibleWorkspaceIds
      ? groupWorkspaces.filter((workspace) =>
          visibleWorkspaceIds.has(workspace.id)
        )
      : [...groupWorkspaces],
  };
}
