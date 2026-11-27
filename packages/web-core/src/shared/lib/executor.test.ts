import { describe, expect, it } from 'vitest';
import { BaseCodingAgent, type Session } from 'shared/types';
import { resolveLatestSessionConfig } from './executor';

function session(id: string, executor: string | null): Session {
  return {
    id,
    workspace_id: 'workspace',
    name: null,
    executor,
    agent_working_dir: null,
    created_at: '2026-11-27T00:00:00Z',
    updated_at: '2026-11-27T00:00:00Z',
  };
}

describe('resolveLatestSessionConfig', () => {
  it('uses the model a CLI session launched with', () => {
    expect(
      resolveLatestSessionConfig({
        processes: [],
        lastSessionProcesses: [],
        session: session('cli', 'CLAUDE_CODE'),
        sessions: [session('cli', 'CLAUDE_CODE')],
        sessionModel: { model_id: 'haiku', reasoning_id: null },
      })
    ).toEqual({
      executor: BaseCodingAgent.CLAUDE_CODE,
      model_id: 'haiku',
      reasoning_id: null,
    });
  });

  it("pairs the model with the open session's own executor", () => {
    const config = resolveLatestSessionConfig({
      processes: [],
      lastSessionProcesses: [],
      session: session('open', 'CLAUDE_CODE'),
      sessions: [session('newer', 'CODEX'), session('open', 'CLAUDE_CODE')],
      sessionModel: { model_id: 'haiku', reasoning_id: null },
    });
    expect(config?.executor).toBe(BaseCodingAgent.CLAUDE_CODE);
  });
});
