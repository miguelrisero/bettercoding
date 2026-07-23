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
