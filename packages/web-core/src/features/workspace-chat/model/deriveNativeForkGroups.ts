import type { NativeFeedEntry, NativeFeedFork } from 'shared/types';

import type {
  PatchTypeWithKey,
  NativeForkDisplayBranch,
  NativeForkDisplayGroup,
} from '@/shared/hooks/useConversationHistory/types';

export interface NativeForkEntryBranch {
  isDefault: boolean;
  entries: PatchTypeWithKey[];
  entryIndices: number[];
}

export interface NativeForkEntrySection {
  key: string;
  forkParentUuid: string;
  prefixEntries: PatchTypeWithKey[];
  branches: NativeForkEntryBranch[];
  insertionIndex: number;
}

function belongsToFork(
  entry: PatchTypeWithKey,
  fork: NativeFeedFork,
  uuids: ReadonlySet<string>,
  branchIndex?: number
): boolean {
  const nativeEntry = entry.nativeEntry;
  const matchesNativeEntry = Boolean(
    nativeEntry?.uuid &&
      nativeEntry.claude_session_id === fork.claude_session_id &&
      uuids.has(nativeEntry.uuid)
  );
  if (matchesNativeEntry) return true;

  return Boolean(
    branchIndex !== undefined &&
      entry.nativeFork?.claudeSessionId === fork.claude_session_id &&
      entry.nativeFork.branch.fork_parent_uuid === fork.fork.fork_parent_uuid &&
      entry.nativeFork.branch.branch_index === branchIndex
  );
}

/** Place a retained executor block inside the native branch that anchors it. */
export function annotateExecutorEntriesWithNativeForks(
  entries: PatchTypeWithKey[],
  nativeEntries: readonly NativeFeedEntry[]
): PatchTypeWithKey[] {
  const branchByProcess = new Map<
    string,
    NonNullable<PatchTypeWithKey['nativeFork']>
  >();

  for (const nativeEntry of nativeEntries) {
    const processId = nativeEntry.linked_execution_process_id;
    if (!processId || !nativeEntry.branch || branchByProcess.has(processId)) {
      continue;
    }
    branchByProcess.set(processId, {
      claudeSessionId: nativeEntry.claude_session_id,
      branch: nativeEntry.branch,
    });
  }

  if (branchByProcess.size === 0) return entries;

  return entries.map((entry) => {
    const nativeFork = branchByProcess.get(entry.executionProcessId);
    return nativeFork ? { ...entry, nativeFork } : entry;
  });
}

/**
 * Match the backend's observed fork metadata to visible native entries.
 * Prefix rows remain in the main timeline; branch descendants are collected
 * for a read-only branch viewer at their first visible position.
 */
export function deriveNativeForkEntrySections(
  entries: readonly PatchTypeWithKey[],
  forks: readonly NativeFeedFork[]
): NativeForkEntrySection[] {
  return forks.flatMap((feedFork) => {
    if (feedFork.fork.branches.length <= 1) return [];

    const prefixUuids = new Set(feedFork.fork.prefix_uuids);
    const prefixEntries = entries.filter((entry) =>
      belongsToFork(entry, feedFork, prefixUuids)
    );
    const branches = feedFork.fork.branches.map((branch, branchIndex) => {
      const nodeUuids = new Set(branch.node_uuids);
      const entryIndices: number[] = [];
      const branchEntries = entries.filter((entry, entryIndex) => {
        const belongs = belongsToFork(entry, feedFork, nodeUuids, branchIndex);
        if (belongs) entryIndices.push(entryIndex);
        return belongs;
      });

      return {
        isDefault: feedFork.fork.default_branch === branchIndex,
        entries: branchEntries,
        entryIndices,
      };
    });
    const branchEntryIndices = branches.flatMap(
      (branch) => branch.entryIndices
    );

    if (branchEntryIndices.length === 0) return [];

    return [
      {
        key: `native-fork:${feedFork.claude_session_id}:${feedFork.fork.fork_parent_uuid}`,
        forkParentUuid: feedFork.fork.fork_parent_uuid,
        prefixEntries,
        branches,
        insertionIndex: Math.min(...branchEntryIndices),
      },
    ];
  });
}

export type ForkPartitionItem =
  | { kind: 'entry'; entry: PatchTypeWithKey }
  | { kind: 'fork'; section: NativeForkEntrySection };

export function partitionEntriesAtNativeForks(
  entries: readonly PatchTypeWithKey[],
  forks: readonly NativeFeedFork[]
): ForkPartitionItem[] {
  const sections = deriveNativeForkEntrySections(entries, forks).sort(
    (left, right) => left.insertionIndex - right.insertionIndex
  );
  if (sections.length === 0) {
    return entries.map((entry) => ({ kind: 'entry', entry }));
  }

  const sectionsByInsertionIndex = new Map<number, NativeForkEntrySection[]>();
  const branchEntryIndices = new Set<number>();

  for (const section of sections) {
    const atIndex = sectionsByInsertionIndex.get(section.insertionIndex) ?? [];
    atIndex.push(section);
    sectionsByInsertionIndex.set(section.insertionIndex, atIndex);
    for (const branch of section.branches) {
      for (const index of branch.entryIndices) branchEntryIndices.add(index);
    }
  }

  const partitioned: ForkPartitionItem[] = [];
  entries.forEach((entry, index) => {
    for (const section of sectionsByInsertionIndex.get(index) ?? []) {
      partitioned.push({ kind: 'fork', section });
    }
    if (!branchEntryIndices.has(index)) {
      partitioned.push({ kind: 'entry', entry });
    }
  });

  return partitioned;
}

export function toNativeForkDisplayGroup(
  section: NativeForkEntrySection,
  branches: NativeForkDisplayBranch[]
): NativeForkDisplayGroup {
  return {
    type: 'NATIVE_FORK_GROUP',
    patchKey: section.key,
    executionProcessId: '',
    forkParentUuid: section.forkParentUuid,
    branches,
  };
}
