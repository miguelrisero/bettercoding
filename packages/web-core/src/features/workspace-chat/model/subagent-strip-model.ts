import type { ToolResult, ToolStatus } from 'shared/types';

import type { PatchTypeWithKey } from '@/shared/hooks/useConversationHistory/types';

export interface SubagentDescriptor {
  key: string;
  name: string | null;
  description: string;
  phase: 'active' | 'done' | 'error';
  result: ToolResult | null;
  status: ToolStatus;
}

export interface StripPartitionInput {
  descriptors: SubagentDescriptor[];
  doneAtByKey: Record<string, number>;
  now: number;
  maxActiveTabs: number;
  lingerMs: number;
}

export interface StripTab {
  descriptor: SubagentDescriptor;
  lingering: boolean;
}

export interface StripPartition {
  tabs: StripTab[];
  drawer: SubagentDescriptor[];
  overflowCount: number;
  overflowLabelMode: 'more' | 'done';
  activeCount: number;
  doneCount: number;
}

function getSubagentPhase(status: ToolStatus): SubagentDescriptor['phase'] {
  switch (status.status) {
    case 'created':
    case 'pending_approval':
      return 'active';
    case 'success':
      return 'done';
    case 'failed':
    case 'denied':
    case 'timed_out':
      return 'error';
    default: {
      const _exhaustive: never = status;
      return _exhaustive;
    }
  }
}

export function selectSubagents(
  entries: PatchTypeWithKey[]
): SubagentDescriptor[] {
  return entries.flatMap((entry) => {
    if (
      entry.type !== 'NORMALIZED_ENTRY' ||
      entry.content.entry_type.type !== 'tool_use' ||
      entry.content.entry_type.action_type.action !== 'task_create'
    ) {
      return [];
    }

    const { action_type: actionType, status } = entry.content.entry_type;

    return [
      {
        key: entry.patchKey,
        name: actionType.subagent_type,
        description: actionType.description,
        phase: getSubagentPhase(status),
        result: actionType.result,
        status,
      },
    ];
  });
}

export function partitionStrip({
  descriptors,
  doneAtByKey,
  now,
  maxActiveTabs,
  lingerMs,
}: StripPartitionInput): StripPartition {
  const active = descriptors.filter(
    (descriptor) => descriptor.phase === 'active'
  );
  const lingering = descriptors.filter((descriptor) => {
    if (descriptor.phase === 'active') return false;

    const doneAt = doneAtByKey[descriptor.key];
    return doneAt !== undefined && now - doneAt < lingerMs;
  });
  const settled = descriptors.filter((descriptor) => {
    if (descriptor.phase === 'active') return false;

    const doneAt = doneAtByKey[descriptor.key];
    return doneAt === undefined || now - doneAt >= lingerMs;
  });

  const budget = Math.max(0, maxActiveTabs);
  const shownActive = active.slice(0, budget);
  const shownLingering = lingering.slice(0, budget - shownActive.length);
  const hiddenActive = active.slice(shownActive.length);
  const hiddenLingering = lingering.slice(shownLingering.length);
  const drawer = [...hiddenActive, ...hiddenLingering, ...settled];

  return {
    tabs: [
      ...shownActive.map((descriptor) => ({
        descriptor,
        lingering: false,
      })),
      ...shownLingering.map((descriptor) => ({
        descriptor,
        lingering: true,
      })),
    ],
    drawer,
    overflowCount: drawer.length,
    overflowLabelMode: hiddenActive.length > 0 ? 'more' : 'done',
    activeCount: active.length,
    doneCount: lingering.length + settled.length,
  };
}
