import type { NativeFeedEntry } from 'shared/types';

export interface ExecutorConversationBlock<T> {
  item: T;
  processIds: readonly string[];
  createdAt: string;
}

export type MergedConversationItem<T> =
  | { kind: 'executor'; item: T }
  | { kind: 'native'; entry: NativeFeedEntry; nativeIndex: number };

interface IndexedNativeEntry {
  entry: NativeFeedEntry;
  originalIndex: number;
}

function compareNativeEntries(
  left: IndexedNativeEntry,
  right: IndexedNativeEntry
): number {
  if (left.entry.seq < right.entry.seq) return -1;
  if (left.entry.seq > right.entry.seq) return 1;
  return left.originalIndex - right.originalIndex;
}

function timestampMs(value: string | null | undefined): number | null {
  if (!value) return null;
  const parsed = new Date(value).getTime();
  return Number.isFinite(parsed) ? parsed : null;
}

/** Native records linked to a process are canonical reconciliation anchors. */
export function isRenderableNativeConversationEntry(
  entry: NativeFeedEntry
): boolean {
  return entry.origin === 'cli' && entry.linked_execution_process_id === null;
}

function findTimestampSlot(
  blockCreatedAt: string,
  entries: readonly IndexedNativeEntry[]
): number {
  const blockTimestamp = timestampMs(blockCreatedAt);
  if (blockTimestamp === null) return entries.length;

  const firstLaterEntry = entries.findIndex((item) => {
    const entryTimestamp = timestampMs(item.entry.ts);
    return entryTimestamp !== null && entryTimestamp >= blockTimestamp;
  });

  return firstLaterEntry === -1 ? entries.length : firstLaterEntry;
}

/**
 * Merge executor process blocks with the canonical native record order.
 *
 * Linked native rows place their process at the corresponding feed boundary,
 * but are not emitted: Phase 1 deliberately keeps executor-rendered content
 * for both running and completed processes. This conservative reconciliation
 * guarantees that the same executor turn is never rendered twice. Unlinked
 * CLI rows remain first-class entries, including native user messages.
 */
export function mergeConversationTimelineItems<T>(
  blocks: readonly ExecutorConversationBlock<T>[],
  nativeEntries: readonly NativeFeedEntry[]
): MergedConversationItem<T>[] {
  if (nativeEntries.length === 0) {
    return blocks.map(({ item }) => ({ kind: 'executor', item }));
  }

  const orderedNativeEntries = nativeEntries
    .map((entry, originalIndex) => ({ entry, originalIndex }))
    .sort(compareNativeEntries);
  const firstNativeIndexByProcess = new Map<string, number>();

  orderedNativeEntries.forEach(({ entry }, index) => {
    const processId = entry.linked_execution_process_id;
    if (processId && !firstNativeIndexByProcess.has(processId)) {
      firstNativeIndexByProcess.set(processId, index);
    }
  });

  const blocksBySlot = new Map<number, T[]>();
  let previousSlot = 0;

  for (const block of blocks) {
    const linkedSlots = block.processIds
      .map((processId) => firstNativeIndexByProcess.get(processId))
      .filter((slot): slot is number => slot !== undefined);
    const rawSlot =
      linkedSlots.length > 0
        ? Math.min(...linkedSlots)
        : findTimestampSlot(block.createdAt, orderedNativeEntries);
    const slot = Math.max(previousSlot, rawSlot);
    previousSlot = slot;

    const slotBlocks = blocksBySlot.get(slot) ?? [];
    slotBlocks.push(block.item);
    blocksBySlot.set(slot, slotBlocks);
  }

  const result: MergedConversationItem<T>[] = [];

  for (let index = 0; index <= orderedNativeEntries.length; index += 1) {
    for (const item of blocksBySlot.get(index) ?? []) {
      result.push({ kind: 'executor', item });
    }

    const indexedEntry = orderedNativeEntries[index];
    if (
      indexedEntry &&
      isRenderableNativeConversationEntry(indexedEntry.entry)
    ) {
      result.push({
        kind: 'native',
        entry: indexedEntry.entry,
        nativeIndex: indexedEntry.originalIndex,
      });
    }
  }

  return result;
}
