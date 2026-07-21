import { aggregateConsecutiveEntries } from '@/shared/lib/aggregateEntries';
import type {
  BaseDisplayEntry,
  DisplayEntry,
  PatchTypeWithKey,
} from '@/shared/hooks/useConversationHistory/types';
import type { NativeFeedFork } from 'shared/types';

import {
  buildConversationRowsIncremental,
  type ConversationRow,
} from './conversation-row-model';
import {
  partitionEntriesAtNativeForks,
  toNativeForkDisplayGroup,
} from './deriveNativeForkGroups';

export interface DerivedConversationTimeline {
  readonly displayEntries: DisplayEntry[];
  readonly rows: ConversationRow[];
}

function isRenderableConversationEntry(entry: BaseDisplayEntry): boolean {
  if (
    entry.type === 'NORMALIZED_ENTRY' &&
    typeof entry.content !== 'string' &&
    'entry_type' in entry.content
  ) {
    const entryType = entry.content.entry_type.type;
    return entryType !== 'next_action' && entryType !== 'token_usage_info';
  }

  return (
    entry.type === 'NORMALIZED_ENTRY' ||
    entry.type === 'STDOUT' ||
    entry.type === 'STDERR' ||
    entry.type === 'AGGREGATED_GROUP' ||
    entry.type === 'AGGREGATED_DIFF_GROUP' ||
    entry.type === 'AGGREGATED_THINKING_GROUP'
  );
}

function aggregateRenderableEntries(
  entries: PatchTypeWithKey[]
): BaseDisplayEntry[] {
  return aggregateConsecutiveEntries(entries).filter(
    isRenderableConversationEntry
  );
}

function deriveForkAwareDisplayEntries(
  entries: PatchTypeWithKey[],
  nativeForks: readonly NativeFeedFork[]
): DisplayEntry[] {
  const partitioned = partitionEntriesAtNativeForks(entries, nativeForks);
  const displayEntries: DisplayEntry[] = [];
  let pendingEntries: PatchTypeWithKey[] = [];

  const flushPendingEntries = () => {
    displayEntries.push(...aggregateRenderableEntries(pendingEntries));
    pendingEntries = [];
  };

  for (const item of partitioned) {
    if (item.kind === 'entry') {
      pendingEntries.push(item.entry);
      continue;
    }

    flushPendingEntries();
    displayEntries.push(
      toNativeForkDisplayGroup(
        item.section,
        item.section.branches.map((branch) => ({
          isDefault: branch.isDefault,
          entries: aggregateRenderableEntries(
            branch.entries.map((entry) => ({
              ...entry,
              executionProcessId: '',
            }))
          ),
        }))
      )
    );
  }

  flushPendingEntries();
  return displayEntries;
}

// Final UI-facing timeline step: aggregate display entries and build stable rows
// for virtualization, navigation, and scroll orchestration.

export function deriveConversationTimeline(
  entries: PatchTypeWithKey[],
  previousDisplayEntries: DisplayEntry[],
  previousRows: ConversationRow[],
  nativeForks: readonly NativeFeedFork[] = []
): DerivedConversationTimeline {
  const displayEntries = deriveForkAwareDisplayEntries(entries, nativeForks);

  const rows = buildConversationRowsIncremental(
    displayEntries,
    previousDisplayEntries,
    previousRows
  );

  return {
    displayEntries,
    rows,
  };
}
