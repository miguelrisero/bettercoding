import { useEffect, useMemo, useState } from 'react';
import {
  CheckCircleIcon,
  FolderOpenIcon,
  GitBranchIcon,
  SpinnerIcon,
  WarningCircleIcon,
  WarningIcon,
  XCircleIcon,
} from '@phosphor-icons/react';
import { useTranslation } from 'react-i18next';
import type { BulkDeleteTarget } from 'shared/types';
import {
  inspectArchivedWorkspaceTargets,
  type BulkDeleteArchivedWorkspaceDetails,
} from '../lib/bulkDeleteArchivedWorkspaces';
import { Button } from './Button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from './KeyboardDialog';

export interface BulkDeleteDialogBranchStatus {
  commitsAhead: number | null;
}

export type BulkDeleteDialogItemOutcome =
  | { status: 'deleted' }
  | { status: 'skipped'; reason: string }
  | { status: 'failed'; reason: string };

export interface BulkDeleteDialogItemResult {
  workspaceId: string;
  workspaceName: string | null;
  outcome: BulkDeleteDialogItemOutcome;
}

export interface BulkDeleteArchivedWorkspacesDialogProps {
  open: boolean;
  bucketLabel: string;
  targets: BulkDeleteTarget[];
  detailsByWorkspaceId: Readonly<
    Record<string, BulkDeleteArchivedWorkspaceDetails>
  >;
  onOpenChange: (open: boolean) => void;
  inspectWorkspace: (
    workspaceId: string
  ) => Promise<BulkDeleteDialogBranchStatus[]>;
  onConfirm: (
    targets: BulkDeleteTarget[]
  ) => Promise<BulkDeleteDialogItemResult[]>;
}

interface InspectionSummary {
  branchCount: number;
  worktreeCount: number;
  unmergedBranchCount: number;
  unmergedWorkspaceCount: number;
  unknownComparisonCount: number;
  inspectionFailureCount: number;
  worktreeAlreadyRemovedCount: number;
}

function OutcomeIcon({
  status,
}: {
  status: BulkDeleteDialogItemOutcome['status'];
}) {
  if (status === 'deleted') {
    return (
      <CheckCircleIcon className="size-4 shrink-0 text-success" weight="fill" />
    );
  }
  if (status === 'skipped') {
    return (
      <WarningCircleIcon
        className="size-4 shrink-0 text-warning"
        weight="fill"
      />
    );
  }
  return <XCircleIcon className="size-4 shrink-0 text-error" weight="fill" />;
}

export function BulkDeleteArchivedWorkspacesDialog({
  open,
  bucketLabel,
  targets,
  detailsByWorkspaceId,
  onOpenChange,
  inspectWorkspace,
  onConfirm,
}: BulkDeleteArchivedWorkspacesDialogProps) {
  const { t } = useTranslation('common');
  const [inspection, setInspection] = useState<InspectionSummary | null>(null);
  const [inspectionError, setInspectionError] = useState<string | null>(null);
  const [inspectionAttempt, setInspectionAttempt] = useState(0);
  const [isInspecting, setIsInspecting] = useState(false);
  const [isDeleting, setIsDeleting] = useState(false);
  const [operationError, setOperationError] = useState<string | null>(null);
  const [results, setResults] = useState<BulkDeleteDialogItemResult[] | null>(
    null
  );

  const initialRepoCount = useMemo(() => {
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
  }, [detailsByWorkspaceId, targets]);

  const initialWorktreeCount = useMemo(() => {
    const targetsWithWorktrees = targets.filter(
      (target) => !detailsByWorkspaceId[target.workspace_id]?.worktreeDeleted
    );
    if (
      targetsWithWorktrees.some(
        (target) =>
          detailsByWorkspaceId[target.workspace_id]?.repoCount === undefined
      )
    ) {
      return null;
    }
    return targetsWithWorktrees.reduce(
      (count, target) =>
        count + (detailsByWorkspaceId[target.workspace_id]?.repoCount ?? 0),
      0
    );
  }, [detailsByWorkspaceId, targets]);

  useEffect(() => {
    if (!open) return;

    let canceled = false;
    setResults(null);
    setOperationError(null);
    setInspection(null);
    setInspectionError(null);
    setIsInspecting(true);

    void inspectArchivedWorkspaceTargets(
      targets,
      inspectWorkspace,
      (workspaceId) =>
        detailsByWorkspaceId[workspaceId]?.worktreeDeleted === true
    )
      .then((workspaceStatuses) => {
        if (canceled) return;

        const branchCount = workspaceStatuses.reduce(
          (count, item) => count + item.statuses.length,
          0
        );
        const unmergedBranchCount = workspaceStatuses.reduce(
          (count, item) =>
            count +
            item.statuses.filter((status) => (status.commitsAhead ?? 0) > 0)
              .length,
          0
        );
        const unmergedWorkspaceCount = workspaceStatuses.filter((item) =>
          item.statuses.some((status) => (status.commitsAhead ?? 0) > 0)
        ).length;
        const unknownComparisonCount = workspaceStatuses.reduce(
          (count, item) =>
            count +
            item.statuses.filter((status) => status.commitsAhead === null)
              .length,
          0
        );
        const inspectionFailureCount = workspaceStatuses.filter(
          (item) => item.inspectionFailed
        ).length;
        const worktreeAlreadyRemovedCount = workspaceStatuses.filter(
          (item) => item.worktreeAlreadyRemoved
        ).length;

        setInspection({
          branchCount,
          worktreeCount: branchCount,
          unmergedBranchCount,
          unmergedWorkspaceCount,
          unknownComparisonCount,
          inspectionFailureCount,
          worktreeAlreadyRemovedCount,
        });
      })
      .catch((error: unknown) => {
        if (canceled) return;
        setInspectionError(
          error instanceof Error
            ? error.message
            : t('kanban.workspaceSidebar.bulkDeleteInspectionFailed', {
                defaultValue: 'Could not inspect every branch.',
              })
        );
      })
      .finally(() => {
        if (!canceled) setIsInspecting(false);
      });

    return () => {
      canceled = true;
    };
  }, [
    detailsByWorkspaceId,
    inspectWorkspace,
    inspectionAttempt,
    open,
    t,
    targets,
  ]);

  const resultCounts = useMemo(() => {
    if (!results) return null;
    return results.reduce(
      (counts, result) => {
        counts[result.outcome.status] += 1;
        return counts;
      },
      { deleted: 0, skipped: 0, failed: 0 }
    );
  }, [results]);

  const handleConfirm = async () => {
    if (!inspection || inspectionError || isInspecting || isDeleting) return;

    setIsDeleting(true);
    setOperationError(null);
    try {
      setResults(await onConfirm(targets));
    } catch (error) {
      setOperationError(
        error instanceof Error
          ? error.message
          : t('kanban.workspaceSidebar.bulkDeleteRequestFailed', {
              defaultValue: 'The bulk removal request failed.',
            })
      );
    } finally {
      setIsDeleting(false);
    }
  };

  const branchCount = initialRepoCount ?? inspection?.branchCount;
  const worktreeCount = initialWorktreeCount ?? inspection?.worktreeCount;

  return (
    <Dialog
      open={open}
      onOpenChange={(nextOpen) => {
        if (!isDeleting) onOpenChange(nextOpen);
      }}
      uncloseable={isDeleting}
    >
      <DialogContent className="sm:max-w-[520px]">
        <DialogHeader>
          <div className="flex items-center gap-base">
            <WarningIcon className="size-6 shrink-0 text-destructive" />
            <DialogTitle>
              {results
                ? t('kanban.workspaceSidebar.bulkDeleteResultsTitle', {
                    defaultValue: 'Removal results',
                  })
                : t('kanban.workspaceSidebar.bulkDeleteTitle', {
                    defaultValue: 'Remove archived workspaces',
                  })}
            </DialogTitle>
          </div>
          <DialogDescription className="pt-half text-left">
            {results
              ? t('kanban.workspaceSidebar.bulkDeleteResultsDescription', {
                  defaultValue:
                    'Each workspace is reported separately so skipped or failed removals are visible.',
                })
              : t('kanban.workspaceSidebar.bulkDeleteDescription', {
                  bucket: bucketLabel,
                  defaultValue:
                    'This permanently removes the workspaces listed below from “{{bucket}}” if their archive state is unchanged. This action cannot be undone.',
                })}
          </DialogDescription>
        </DialogHeader>

        {results && resultCounts ? (
          <div className="flex flex-col gap-base" aria-live="polite">
            <p className="text-sm font-medium text-normal tabular-nums">
              {t('kanban.workspaceSidebar.bulkDeleteResultSummary', {
                deleted: resultCounts.deleted,
                skipped: resultCounts.skipped,
                failed: resultCounts.failed,
                defaultValue:
                  '{{deleted}} removed, {{skipped}} skipped, {{failed}} failed',
              })}
            </p>
            <div className="max-h-64 overflow-y-auto border-y border-border py-half">
              {results.map((result) => (
                <div
                  key={result.workspaceId}
                  className="flex items-start gap-half px-half py-half text-sm"
                >
                  <OutcomeIcon status={result.outcome.status} />
                  <div className="min-w-0 flex-1">
                    <div className="flex items-baseline justify-between gap-base">
                      <span className="truncate text-normal">
                        {result.workspaceName ??
                          detailsByWorkspaceId[result.workspaceId]
                            ?.workspaceName ??
                          result.workspaceId}
                      </span>
                      <span className="shrink-0 text-xs capitalize text-low">
                        {result.outcome.status}
                      </span>
                    </div>
                    {'reason' in result.outcome && (
                      <p className="mt-0.5 text-xs text-low">
                        {result.outcome.reason}
                      </p>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </div>
        ) : (
          <form
            className="flex flex-col gap-base"
            onSubmit={(event) => {
              event.preventDefault();
              void handleConfirm();
            }}
          >
            <div className="border border-destructive/30 bg-destructive/5 p-base">
              <dl className="grid grid-cols-[1fr_auto] gap-x-base gap-y-half text-sm">
                <dt className="text-low">
                  {t('kanban.workspaceSidebar.bulkDeleteWorkspaces', {
                    defaultValue: 'Workspaces',
                  })}
                </dt>
                <dd className="text-right font-medium tabular-nums text-normal">
                  {targets.length}
                </dd>
                <dt className="flex items-center gap-half text-low">
                  <GitBranchIcon className="size-4" />
                  {t('kanban.workspaceSidebar.bulkDeleteBranches', {
                    defaultValue: 'Branches deleted',
                  })}
                </dt>
                <dd className="text-right font-medium tabular-nums text-normal">
                  {branchCount ?? '—'}
                </dd>
                <dt className="flex items-center gap-half text-low">
                  <FolderOpenIcon className="size-4" />
                  {t('kanban.workspaceSidebar.bulkDeleteWorktrees', {
                    defaultValue: 'Worktrees removed',
                  })}
                </dt>
                <dd className="text-right font-medium tabular-nums text-normal">
                  {worktreeCount ?? '—'}
                </dd>
                <dt className="text-low">
                  {t('kanban.workspaceSidebar.bulkDeleteUnmerged', {
                    defaultValue: 'Branches with unmerged commits',
                  })}
                </dt>
                <dd className="text-right font-medium tabular-nums text-normal">
                  {isInspecting ? (
                    <span className="inline-flex items-center gap-half text-low">
                      <SpinnerIcon className="size-4 animate-spin" />
                      {t('kanban.workspaceSidebar.bulkDeleteInspecting', {
                        defaultValue: 'Inspecting…',
                      })}
                    </span>
                  ) : inspection ? (
                    <span>
                      {inspection.unmergedBranchCount}
                      <span className="ml-half text-xs font-normal text-low">
                        {t(
                          'kanban.workspaceSidebar.bulkDeleteUnmergedWorkspaces',
                          {
                            count: inspection.unmergedWorkspaceCount,
                            defaultValue: 'across {{count}} workspace(s)',
                          }
                        )}
                      </span>
                    </span>
                  ) : (
                    '—'
                  )}
                </dd>
                {inspection && inspection.unknownComparisonCount > 0 && (
                  <>
                    <dt className="text-warning">
                      {t(
                        'kanban.workspaceSidebar.bulkDeleteUnknownComparison',
                        {
                          defaultValue: 'Branches that could not be compared',
                        }
                      )}
                    </dt>
                    <dd className="text-right font-medium tabular-nums text-warning">
                      {inspection.unknownComparisonCount}
                    </dd>
                  </>
                )}
                {inspection && inspection.inspectionFailureCount > 0 && (
                  <>
                    <dt className="text-warning">
                      {t(
                        'kanban.workspaceSidebar.bulkDeleteInspectionFailures',
                        {
                          defaultValue:
                            'Workspaces whose unmerged status is unknown',
                        }
                      )}
                    </dt>
                    <dd className="text-right font-medium tabular-nums text-warning">
                      {inspection.inspectionFailureCount}
                    </dd>
                  </>
                )}
                {inspection && inspection.worktreeAlreadyRemovedCount > 0 && (
                  <>
                    <dt className="text-warning">
                      {t(
                        'kanban.workspaceSidebar.bulkDeleteWorktreeAlreadyRemoved',
                        {
                          defaultValue:
                            'Unmerged status unknown, worktree already removed',
                        }
                      )}
                    </dt>
                    <dd className="text-right font-medium tabular-nums text-warning">
                      {inspection.worktreeAlreadyRemovedCount}
                    </dd>
                  </>
                )}
              </dl>
            </div>

            {inspectionError && (
              <div className="flex items-start justify-between gap-base text-sm text-error">
                <span>{inspectionError}</span>
                <Button
                  type="button"
                  variant="outline"
                  size="xs"
                  onClick={() => setInspectionAttempt((attempt) => attempt + 1)}
                >
                  {t('kanban.workspaceSidebar.bulkDeleteRetry', {
                    defaultValue: 'Retry',
                  })}
                </Button>
              </div>
            )}

            <div>
              <p className="mb-half text-xs text-low">
                {t('kanban.workspaceSidebar.bulkDeleteTargets', {
                  defaultValue: 'Workspaces that will be removed',
                })}
              </p>
              <ul className="max-h-32 overflow-y-auto border-y border-border py-half text-sm text-low">
                {targets.map((target) => (
                  <li
                    key={target.workspace_id}
                    className="truncate px-half py-0.5"
                  >
                    {detailsByWorkspaceId[target.workspace_id]?.workspaceName ??
                      target.workspace_id}
                  </li>
                ))}
              </ul>
            </div>

            {operationError && (
              <p className="text-sm text-error" role="alert">
                {operationError}
              </p>
            )}

            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                onClick={() => onOpenChange(false)}
                disabled={isDeleting}
              >
                {t('buttons.cancel')}
              </Button>
              <Button
                type="submit"
                variant="destructive"
                disabled={
                  isInspecting ||
                  !inspection ||
                  Boolean(inspectionError) ||
                  isDeleting
                }
              >
                {isDeleting && (
                  <SpinnerIcon className="mr-half size-4 animate-spin" />
                )}
                {isDeleting
                  ? t('kanban.workspaceSidebar.bulkDeleteRemoving', {
                      defaultValue: 'Removing…',
                    })
                  : t('kanban.workspaceSidebar.bulkDeleteConfirm', {
                      count: targets.length,
                      defaultValue: 'Remove {{count}} workspaces',
                    })}
              </Button>
            </DialogFooter>
          </form>
        )}

        {results && (
          <DialogFooter>
            <Button variant="outline" onClick={() => onOpenChange(false)}>
              {t('buttons.close')}
            </Button>
          </DialogFooter>
        )}
      </DialogContent>
    </Dialog>
  );
}
