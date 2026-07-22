import type { BulkDeleteTarget } from 'shared/types';

const MAX_INSPECTION_CONCURRENCY = 8;

export interface ArchivedWorkspaceBulkDeleteSource {
  id: string;
  name: string;
  archivedAt: string | null;
  worktreeDeleted?: boolean;
  repoCount?: number;
}

export interface BulkDeleteArchivedWorkspaceDetails {
  workspaceName: string;
  repoCount?: number;
  worktreeDeleted: boolean;
}

export function sumRepoCounts(
  targets: readonly BulkDeleteTarget[],
  detailsByWorkspaceId: Readonly<
    Record<string, BulkDeleteArchivedWorkspaceDetails>
  >
): number | null {
  if (
    targets.some(
      (target) =>
        detailsByWorkspaceId[target.workspace_id]?.repoCount === undefined
    )
  ) {
    return null;
  }

  return targets.reduce(
    (count, target) =>
      count + (detailsByWorkspaceId[target.workspace_id]?.repoCount ?? 0),
    0
  );
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
          worktreeDeleted: workspace.worktreeDeleted === true,
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

export interface ArchivedWorkspaceInspectionResult<TStatus> {
  target: BulkDeleteTarget;
  statuses: TStatus[];
  inspectionFailed: boolean;
  worktreeAlreadyRemoved: boolean;
}

export async function inspectArchivedWorkspaceTargets<TStatus>(
  targets: readonly BulkDeleteTarget[],
  inspectWorkspace: (workspaceId: string) => Promise<TStatus[]>,
  isWorktreeAlreadyRemoved: (workspaceId: string) => boolean = () => false
): Promise<ArchivedWorkspaceInspectionResult<TStatus>[]> {
  const results: ArchivedWorkspaceInspectionResult<TStatus>[] = new Array(
    targets.length
  );
  let nextIndex = 0;

  const workers = Array.from(
    {
      length: Math.min(MAX_INSPECTION_CONCURRENCY, targets.length),
    },
    async () => {
      while (nextIndex < targets.length) {
        const index = nextIndex;
        nextIndex += 1;
        const target = targets[index];

        if (isWorktreeAlreadyRemoved(target.workspace_id)) {
          results[index] = {
            target,
            statuses: [],
            inspectionFailed: false,
            worktreeAlreadyRemoved: true,
          };
          continue;
        }

        try {
          results[index] = {
            target,
            statuses: await inspectWorkspace(target.workspace_id),
            inspectionFailed: false,
            worktreeAlreadyRemoved: false,
          };
        } catch {
          results[index] = {
            target,
            statuses: [],
            inspectionFailed: true,
            worktreeAlreadyRemoved: false,
          };
        }
      }
    }
  );

  await Promise.all(workers);
  return results;
}
