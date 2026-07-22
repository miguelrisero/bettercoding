import { describe, expect, it } from 'vitest';
import { archiveBucketForTimestamp } from './archiveBuckets';
import {
  buildArchivedBucketState,
  inspectArchivedWorkspaceTargets,
} from './bulkDeleteArchivedWorkspaces';

describe('buildArchivedBucketState', () => {
  it('builds targets from the full bucket despite search and pagination visibility', () => {
    const fullBucket = Array.from({ length: 55 }, (_, index) => ({
      id: `workspace-${index}`,
      name: `Workspace ${index}`,
      archivedAt: index === 54 ? null : '2026-06-01T12:00:00Z',
    }));
    const paginatedIds = new Set(
      fullBucket.slice(0, 50).map((workspace) => workspace.id)
    );
    const searchMatchedIds = new Set(
      fullBucket
        .filter((workspace) => workspace.name.endsWith('1'))
        .map((workspace) => workspace.id)
        .filter((workspaceId) => paginatedIds.has(workspaceId))
    );

    const { targets, detailsByWorkspaceId, visibleWorkspaces } =
      buildArchivedBucketState(fullBucket, searchMatchedIds);

    expect(visibleWorkspaces.map((workspace) => workspace.id)).toEqual([
      'workspace-1',
      'workspace-11',
      'workspace-21',
      'workspace-31',
      'workspace-41',
    ]);
    expect(targets).toHaveLength(fullBucket.length);
    expect(targets.map((target) => target.workspace_id)).toEqual(
      fullBucket.map((workspace) => workspace.id)
    );
    expect(targets.at(-1)?.archived_at).toBeNull();
    expect(detailsByWorkspaceId['workspace-54'].workspaceName).toBe(
      'Workspace 54'
    );
    expect(
      archiveBucketForTimestamp(
        fullBucket[54].archivedAt,
        Date.UTC(2026, 6, 22, 12)
      )
    ).toBe('older_than_thirty_days');
  });
});

describe('inspectArchivedWorkspaceTargets', () => {
  it('preserves successful results when one workspace inspection fails', async () => {
    const targets = ['first', 'failing', 'last'].map((workspaceId) => ({
      workspace_id: workspaceId,
      archived_at: '2026-06-01T12:00:00Z',
    }));

    const results = await inspectArchivedWorkspaceTargets(
      targets,
      async (workspaceId) => {
        if (workspaceId === 'failing') {
          throw new Error('transient inspection failure');
        }
        return [{ commitsAhead: workspaceId === 'first' ? 1 : 0 }];
      }
    );

    expect(results).toEqual([
      {
        target: targets[0],
        statuses: [{ commitsAhead: 1 }],
        inspectionFailed: false,
        worktreeAlreadyRemoved: false,
      },
      {
        target: targets[1],
        statuses: [],
        inspectionFailed: true,
        worktreeAlreadyRemoved: false,
      },
      {
        target: targets[2],
        statuses: [{ commitsAhead: 0 }],
        inspectionFailed: false,
        worktreeAlreadyRemoved: false,
      },
    ]);
  });

  it('does not inspect a workspace whose worktree is already removed', async () => {
    const targets = ['removed', 'existing'].map((workspaceId) => ({
      workspace_id: workspaceId,
      archived_at: '2026-06-01T12:00:00Z',
    }));
    const inspectedWorkspaceIds: string[] = [];

    const results = await inspectArchivedWorkspaceTargets(
      targets,
      async (workspaceId) => {
        inspectedWorkspaceIds.push(workspaceId);
        return [{ commitsAhead: 0 }];
      },
      (workspaceId) => workspaceId === 'removed'
    );

    expect(inspectedWorkspaceIds).toEqual(['existing']);
    expect(results[0]).toEqual({
      target: targets[0],
      statuses: [],
      inspectionFailed: false,
      worktreeAlreadyRemoved: true,
    });
    expect(results[1].worktreeAlreadyRemoved).toBe(false);
  });
});
