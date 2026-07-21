import { describe, expect, it } from 'vitest';
import type { NativeFeedEntry, NativeFeedOrigin } from 'shared/types';

import {
  isRenderableNativeConversationEntry,
  mergeConversationTimelineItems,
  type ExecutorConversationBlock,
} from './mergeConversationTimeline';

function nativeEntry({
  seq,
  content,
  origin = 'cli',
  linkedProcessId = null,
  uuid = `uuid-${seq}`,
  parentUuid = null,
}: {
  seq: number;
  content: string;
  origin?: NativeFeedOrigin;
  linkedProcessId?: string | null;
  uuid?: string;
  parentUuid?: string | null;
}): NativeFeedEntry {
  return {
    normalized_entry: {
      entry_type: { type: 'assistant_message' },
      content,
      timestamp: null,
    },
    claude_session_id: 'claude-session',
    uuid,
    parent_uuid: parentUuid,
    ts: `2026-07-20T00:00:${String(seq).padStart(2, '0')}.000Z`,
    origin,
    linked_execution_process_id: linkedProcessId,
    git_branch: null,
    version: null,
    branch: null,
    seq: BigInt(seq),
  };
}

function block(
  item: string,
  processId: string,
  second: number
): ExecutorConversationBlock<string> {
  return {
    item,
    processIds: [processId],
    createdAt: `2026-07-20T00:00:${String(second).padStart(2, '0')}.000Z`,
  };
}

function labels(
  merged: ReturnType<typeof mergeConversationTimelineItems<string>>
) {
  return merged.map((item) =>
    item.kind === 'executor' ? item.item : item.entry.normalized_entry.content
  );
}

describe('mergeConversationTimelineItems', () => {
  it('orders a CLI-only conversation by feed sequence', () => {
    const parent = nativeEntry({ seq: 1, content: 'first', uuid: 'parent' });
    const child = nativeEntry({
      seq: 2,
      content: 'second',
      uuid: 'child',
      parentUuid: 'parent',
    });

    expect(labels(mergeConversationTimelineItems([], [child, parent]))).toEqual(
      ['first', 'second']
    );
  });

  it('preserves executor-only behavior when there is no native feed', () => {
    const blocks = [
      block('executor one', 'process-1', 1),
      block('executor two', 'process-2', 2),
    ];

    expect(labels(mergeConversationTimelineItems(blocks, []))).toEqual([
      'executor one',
      'executor two',
    ]);
  });

  it('places CLI turns between executor process boundaries', () => {
    const blocks = [
      block('executor one', 'process-1', 1),
      block('executor two', 'process-2', 3),
    ];
    const feed = [
      nativeEntry({
        seq: 10,
        content: 'linked one',
        origin: 'executor',
        linkedProcessId: 'process-1',
      }),
      nativeEntry({ seq: 20, content: 'CLI between runs' }),
      nativeEntry({
        seq: 30,
        content: 'linked two',
        origin: 'executor',
        linkedProcessId: 'process-2',
      }),
    ];

    expect(labels(mergeConversationTimelineItems(blocks, feed))).toEqual([
      'executor one',
      'CLI between runs',
      'executor two',
    ]);
  });

  it('never emits a linked native copy of an executor turn', () => {
    const merged = mergeConversationTimelineItems(
      [block('executor overlay', 'process-1', 1)],
      [
        nativeEntry({
          seq: 1,
          content: 'linked app prompt',
          origin: 'app',
          linkedProcessId: 'process-1',
        }),
        nativeEntry({
          seq: 2,
          content: 'linked executor answer',
          origin: 'executor',
          linkedProcessId: 'process-1',
        }),
        nativeEntry({ seq: 3, content: 'independent CLI turn' }),
      ]
    );

    expect(labels(merged)).toEqual([
      'executor overlay',
      'independent CLI turn',
    ]);
    expect(merged.filter((item) => item.kind === 'executor')).toHaveLength(1);
  });
});

describe('isRenderableNativeConversationEntry', () => {
  it('only admits unlinked CLI entries', () => {
    expect(
      isRenderableNativeConversationEntry(
        nativeEntry({ seq: 1, content: 'CLI' })
      )
    ).toBe(true);
    expect(
      isRenderableNativeConversationEntry(
        nativeEntry({
          seq: 2,
          content: 'linked',
          linkedProcessId: 'process-1',
        })
      )
    ).toBe(false);
    expect(
      isRenderableNativeConversationEntry(
        nativeEntry({ seq: 3, content: 'app', origin: 'app' })
      )
    ).toBe(false);
  });
});
