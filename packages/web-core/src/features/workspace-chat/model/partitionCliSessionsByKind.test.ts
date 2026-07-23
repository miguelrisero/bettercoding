import { describe, expect, it } from 'vitest';
import type { CliSessionKind, UnassignedCliSession } from 'shared/types';

import { partitionCliSessionsByKind } from './partitionCliSessionsByKind';

function session(
  claudeSessionId: string,
  kind: CliSessionKind
): UnassignedCliSession {
  return {
    claude_session_id: claudeSessionId,
    cwd: '/workspace',
    dir_path: '/transcripts',
    file_name: `${claudeSessionId}.jsonl`,
    mtime_ms: null,
    first_prompt_snippet: null,
    kind,
  };
}

describe('partitionCliSessionsByKind', () => {
  it('splits mixed sessions while preserving order', () => {
    const mainOne = session('main-one', 'main');
    const agentOne = session('agent-one', 'subagent');
    const mainTwo = session('main-two', 'main');
    const agentTwo = session('agent-two', 'subagent');

    expect(
      partitionCliSessionsByKind([mainOne, agentOne, mainTwo, agentTwo])
    ).toEqual({
      main: [mainOne, mainTwo],
      agents: [agentOne, agentTwo],
    });
  });

  it('keeps all main sessions visible', () => {
    const sessions = [session('main-one', 'main'), session('main-two', 'main')];

    expect(partitionCliSessionsByKind(sessions)).toEqual({
      main: sessions,
      agents: [],
    });
  });

  it('puts all positively identified subagents in agents', () => {
    const sessions = [
      session('agent-one', 'subagent'),
      session('agent-two', 'subagent'),
    ];

    expect(partitionCliSessionsByKind(sessions)).toEqual({
      main: [],
      agents: sessions,
    });
  });

  it('returns empty partitions for an empty list', () => {
    expect(partitionCliSessionsByKind([])).toEqual({
      main: [],
      agents: [],
    });
  });

  it('fails open to main for unexpected or absent kinds', () => {
    const unexpected = {
      ...session('unexpected', 'main'),
      kind: 'future-kind',
    } as unknown as UnassignedCliSession;
    const absent = {
      ...session('absent', 'main'),
      kind: undefined,
    } as unknown as UnassignedCliSession;

    expect(partitionCliSessionsByKind([unexpected, absent])).toEqual({
      main: [unexpected, absent],
      agents: [],
    });
  });
});
