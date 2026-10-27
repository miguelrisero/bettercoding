import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/providers/HostIdProvider', () => ({
  getCurrentHostId: () => null,
}));
vi.mock('@/shared/lib/remoteApi', () => ({ makeRequest: vi.fn() }));

import { cliAgentApi } from './api';
import { setLocalApiTransport } from './localApiTransport';

afterEach(() => {
  setLocalApiTransport(null);
  vi.restoreAllMocks();
});

describe('explicit CLI conversation handover', () => {
  it('replaces the binding before launching the selected conversation', async () => {
    const paths: string[] = [];
    setLocalApiTransport({
      request: async (path, options) => {
        expect(options?.method).toBe('POST');
        paths.push(path);
        if (path.includes('/session/')) {
          expect(JSON.parse(options?.body as string)).toEqual({
            acknowledge_agent_shutdown: true,
            expected_binding_id: 'binding-shown-before-confirmation',
          });
        }
        return Response.json({ success: true, data: null });
      },
      openWebSocket: () => {
        throw new Error('unexpected WebSocket');
      },
    });
    await cliAgentApi.switchSession('workspace', 'selected-session', {
      acknowledge_agent_shutdown: true,
      expected_binding_id: 'binding-shown-before-confirmation',
    });
    expect(paths).toEqual([
      '/api/workspaces/workspace/cli/session/selected-session',
      '/api/workspaces/workspace/cli/launch',
    ]);
  });

  it('leaves the pane unlaunched when the switch fails', async () => {
    vi.spyOn(console, 'error').mockImplementation(() => {});
    const request = vi.fn(async () =>
      Response.json({ message: 'writer busy' }, { status: 409 })
    );
    setLocalApiTransport({
      request,
      openWebSocket: () => {
        throw new Error('unexpected WebSocket');
      },
    });
    await expect(
      cliAgentApi.switchSession('workspace', 'selected-session', {
        acknowledge_agent_shutdown: true,
        expected_binding_id: null,
      })
    ).rejects.toThrow('writer busy');
    expect(request).toHaveBeenCalledTimes(1);
  });
});
