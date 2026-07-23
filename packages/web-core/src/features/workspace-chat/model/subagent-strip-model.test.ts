// @vitest-environment node

import { describe, expect, it } from 'vitest';
import type { ToolResult, ToolStatus } from 'shared/types';

import type { PatchTypeWithKey } from '@/shared/hooks/useConversationHistory/types';
import { selectSubagents } from './subagent-strip-model';

const markdownResult: ToolResult = {
  type: { type: 'markdown' },
  value: 'Finished result',
};

function taskCreateEntry({
  key,
  status,
  name = 'researcher',
  description = `Task ${key}`,
  result = null,
}: {
  key: string;
  status: ToolStatus;
  name?: string | null;
  description?: string;
  result?: ToolResult | null;
}): PatchTypeWithKey {
  return {
    type: 'NORMALIZED_ENTRY',
    content: {
      timestamp: null,
      content: '',
      entry_type: {
        type: 'tool_use',
        tool_name: 'Task',
        action_type: {
          action: 'task_create',
          description,
          subagent_type: name,
          result,
        },
        status,
      },
    },
    patchKey: key,
    executionProcessId: 'process-1',
  };
}

describe('selectSubagents', () => {
  it('returns an empty projection for no entries', () => {
    expect(selectSubagents([])).toEqual([]);
  });

  it('projects a single active task_create entry', () => {
    const status: ToolStatus = { status: 'created' };

    expect(
      selectSubagents([
        taskCreateEntry({
          key: 'task-1',
          status,
          description: 'Inspect the test suite',
        }),
      ])
    ).toEqual([
      {
        key: 'task-1',
        name: 'researcher',
        description: 'Inspect the test suite',
        phase: 'active',
        result: null,
        status,
      },
    ]);
  });

  it('maps active, done, and error statuses in spawn order', () => {
    const entries = [
      taskCreateEntry({
        key: 'active',
        status: {
          status: 'pending_approval',
          approval_id: 'approval-1',
        },
      }),
      taskCreateEntry({
        key: 'done',
        status: { status: 'success' },
        result: markdownResult,
      }),
      taskCreateEntry({
        key: 'error',
        status: { status: 'timed_out' },
      }),
    ];

    expect(
      selectSubagents(entries).map(({ key, phase }) => [key, phase])
    ).toEqual([
      ['active', 'active'],
      ['done', 'done'],
      ['error', 'error'],
    ]);
    expect(selectSubagents(entries)[1].result).toBe(markdownResult);
  });

  it('preserves a null subagent type for the localized view fallback', () => {
    const [descriptor] = selectSubagents([
      taskCreateEntry({
        key: 'unnamed',
        name: null,
        status: { status: 'success' },
      }),
    ]);

    expect(descriptor.name).toBeNull();
  });

  it('ignores entries that are not task_create tool uses', () => {
    const otherTool: PatchTypeWithKey = {
      type: 'NORMALIZED_ENTRY',
      content: {
        timestamp: null,
        content: '',
        entry_type: {
          type: 'tool_use',
          tool_name: 'Read',
          action_type: { action: 'file_read', path: 'README.md' },
          status: { status: 'success' },
        },
      },
      patchKey: 'read-1',
      executionProcessId: 'process-1',
    };
    const stdout: PatchTypeWithKey = {
      type: 'STDOUT',
      content: 'not a normalized entry',
      patchKey: 'stdout-1',
      executionProcessId: 'process-1',
    };

    expect(selectSubagents([otherTool, stdout])).toEqual([]);
  });
});
