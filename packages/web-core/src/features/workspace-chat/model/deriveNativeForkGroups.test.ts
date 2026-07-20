import { describe, expect, it } from 'vitest';
import type {
  NativeBranchMetadata,
  NativeFeedEntry,
  NativeFeedFork,
  NormalizedEntry,
} from 'shared/types';

import type { PatchTypeWithKey } from '@/shared/hooks/useConversationHistory/types';
import {
  annotateExecutorEntriesWithNativeForks,
  deriveNativeForkEntrySections,
  partitionEntriesAtNativeForks,
} from './deriveNativeForkGroups';

const CLAUDE_SESSION_ID = 'claude-session';
const FORK_PARENT_UUID = 'fork-parent';

function normalized(content: string): NormalizedEntry {
  return {
    entry_type: { type: 'assistant_message' },
    content,
    timestamp: null,
  };
}

function branchMetadata(
  branchIndex: number,
  isDefault: boolean
): NativeBranchMetadata {
  return {
    fork_parent_uuid: FORK_PARENT_UUID,
    branch_index: branchIndex,
    branch_label: `Branch ${branchIndex + 1}`,
    is_default: isDefault,
  };
}

function nativeEntry(
  uuid: string,
  content: string,
  seq: number,
  branch: NativeBranchMetadata | null,
  linkedProcessId: string | null = null
): NativeFeedEntry {
  return {
    normalized_entry: normalized(content),
    claude_session_id: CLAUDE_SESSION_ID,
    uuid,
    parent_uuid: null,
    ts: null,
    origin: linkedProcessId ? 'executor' : 'cli',
    linked_execution_process_id: linkedProcessId,
    git_branch: null,
    version: null,
    branch,
    seq: BigInt(seq),
  };
}

function nativePatch(entry: NativeFeedEntry): PatchTypeWithKey {
  return {
    type: 'NORMALIZED_ENTRY',
    content: entry.normalized_entry,
    patchKey: `native:${entry.uuid}`,
    executionProcessId: '',
    nativeEntry: entry,
  };
}

function executorPatch(processId: string): PatchTypeWithKey {
  return {
    type: 'NORMALIZED_ENTRY',
    content: normalized('executor-rendered branch'),
    patchKey: `${processId}:0`,
    executionProcessId: processId,
  };
}

function feedFork(): NativeFeedFork {
  return {
    claude_session_id: CLAUDE_SESSION_ID,
    file_id: 'file-id',
    fork: {
      fork_parent_uuid: FORK_PARENT_UUID,
      prefix_uuids: ['root', FORK_PARENT_UUID],
      branches: [
        {
          label: 'Branch 1',
          root_uuid: 'branch-1-root',
          node_uuids: ['branch-1-root'],
          leaf_uuids: ['branch-1-root'],
        },
        {
          label: 'Branch 2',
          root_uuid: 'branch-2-root',
          node_uuids: ['branch-2-root'],
          leaf_uuids: ['branch-2-root'],
        },
      ],
      default_branch: 1,
    },
  };
}

describe('deriveNativeForkEntrySections', () => {
  it('keeps the common prefix inline and computes labeled branches', () => {
    const entries = [
      nativePatch(nativeEntry('root', 'common root', 1, null)),
      nativePatch(nativeEntry(FORK_PARENT_UUID, 'fork parent', 2, null)),
      nativePatch(
        nativeEntry(
          'branch-1-root',
          'alternate path',
          3,
          branchMetadata(0, false)
        )
      ),
      nativePatch(
        nativeEntry('branch-2-root', 'resume path', 4, branchMetadata(1, true))
      ),
    ];

    const [section] = deriveNativeForkEntrySections(entries, [feedFork()]);

    expect(section.prefixEntries.map((entry) => entry.patchKey)).toEqual([
      'native:root',
      `native:${FORK_PARENT_UUID}`,
    ]);
    expect(
      section.branches.map((branch) => ({
        label: branch.label,
        isDefault: branch.isDefault,
        entries: branch.entries.map((entry) => entry.patchKey),
      }))
    ).toEqual([
      {
        label: 'Branch 1',
        isDefault: false,
        entries: ['native:branch-1-root'],
      },
      {
        label: 'Branch 2',
        isDefault: true,
        entries: ['native:branch-2-root'],
      },
    ]);

    const partition = partitionEntriesAtNativeForks(entries, [feedFork()]);
    expect(partition.map((item) => item.kind)).toEqual([
      'entry',
      'entry',
      'fork',
    ]);
  });

  it('places a retained executor rendering in its linked native branch', () => {
    const processId = 'process-2';
    const linkedNativeEntry = nativeEntry(
      'branch-2-root',
      'linked native duplicate',
      4,
      branchMetadata(1, true),
      processId
    );
    const annotated = annotateExecutorEntriesWithNativeForks(
      [executorPatch(processId)],
      [linkedNativeEntry]
    );
    const [section] = deriveNativeForkEntrySections(annotated, [feedFork()]);

    expect(section.branches[0].entries).toEqual([]);
    expect(section.branches[1].entries.map((entry) => entry.patchKey)).toEqual([
      `${processId}:0`,
    ]);
  });
});
