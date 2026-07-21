import { describe, expect, it } from 'vitest';
import {
  BaseCodingAgent,
  type NativeFeedEntry,
  type NormalizedEntry,
} from 'shared/types';

import type {
  ConversationTimelineSource,
  PatchTypeWithKey,
} from '@/shared/hooks/useConversationHistory/types';
import { deriveConversationEntries } from './deriveConversationEntries';

function normalizedUser(content: string): NormalizedEntry {
  return {
    entry_type: { type: 'user_message' },
    content,
    timestamp: null,
  };
}

function patch(
  entry: NormalizedEntry,
  processId: string,
  index: number
): PatchTypeWithKey {
  return {
    type: 'NORMALIZED_ENTRY',
    content: entry,
    patchKey: `${processId}:${index}`,
    executionProcessId: processId,
  };
}

function nativeUserEntry(content: string): NativeFeedEntry {
  return {
    normalized_entry: normalizedUser(content),
    claude_session_id: 'claude-session',
    uuid: 'native-user',
    parent_uuid: null,
    ts: '2026-07-20T00:00:01.000Z',
    origin: 'cli',
    linked_execution_process_id: null,
    git_branch: null,
    version: null,
    branch: null,
    seq: 1n,
  };
}

function visibleUserEntries(entries: PatchTypeWithKey[]) {
  return entries.filter(
    (entry) =>
      entry.type === 'NORMALIZED_ENTRY' &&
      entry.content.entry_type.type === 'user_message'
  );
}

describe('deriveConversationEntries user messages', () => {
  it('renders a native CLI user_message as a first-class row', () => {
    const nativeEntry = nativeUserEntry('typed in the CLI');
    const source: ConversationTimelineSource = {
      executionProcessState: {},
      liveExecutionProcesses: [],
      nativeFeed: {
        revision: 1n,
        seq: 1n,
        entries: [nativeEntry],
        forks: [],
      },
    };

    const result = deriveConversationEntries({
      source,
      scriptOutputCache: new Map(),
    });
    const userEntries = visibleUserEntries(result.entries);

    expect(userEntries).toHaveLength(1);
    expect(userEntries[0].content.content).toBe('typed in the CLI');
    expect(userEntries[0].nativeEntry).toBe(nativeEntry);
    expect(userEntries[0].executionProcessId).toBe('');
  });

  it('keeps executor prompt synthesis and filters executor user log rows', () => {
    const processId = 'process-1';
    const source: ConversationTimelineSource = {
      executionProcessState: {
        [processId]: {
          executionProcess: {
            id: processId,
            created_at: '2026-07-20T00:00:01.000Z',
            updated_at: '2026-07-20T00:00:02.000Z',
            executor_action: {
              typ: {
                type: 'CodingAgentInitialRequest',
                prompt: 'prompt submitted by the app',
                executor_config: {
                  executor: BaseCodingAgent.CLAUDE_CODE,
                },
                working_dir: null,
              },
              next_action: null,
            },
          },
          entries: [
            patch(normalizedUser('internal executor user row'), processId, 0),
            patch(
              {
                entry_type: { type: 'assistant_message' },
                content: 'assistant answer',
                timestamp: null,
              },
              processId,
              1
            ),
          ],
        },
      },
      liveExecutionProcesses: [],
    };

    const result = deriveConversationEntries({
      source,
      scriptOutputCache: new Map(),
    });

    expect(
      visibleUserEntries(result.entries).map((entry) => entry.content.content)
    ).toEqual(['prompt submitted by the app']);
    expect(
      result.entries.some(
        (entry) =>
          entry.type === 'NORMALIZED_ENTRY' &&
          entry.content.content === 'internal executor user row'
      )
    ).toBe(false);
  });
});
