import { afterEach, describe, expect, it, vi } from 'vitest';
import { setLocalApiTransport } from '@/shared/lib/localApiTransport';
import { inputsApi, refreshInputAfterConflict } from './inputsApi';

afterEach(() => setLocalApiTransport(null));

describe('Input status actions', () => {
  it.each(['VERSION_CONFLICT', 'SEQUENCE_CONFLICT: current_sequence=9'])(
    'refetches current metadata after %s without another write',
    async (code) => {
      const request = vi.fn(
        async () =>
          new Response(JSON.stringify({ success: false, message: code }), {
            status: 409,
          })
      );
      setLocalApiTransport({ request, openWebSocket: vi.fn() });
      const refresh = vi.fn(async () => {});
      const body = {
        request_id: 'answer',
        expected_sequence: 3,
        version: 2,
        answer: { choice_id: 'ship', text: 'Saved draft' },
      };
      try {
        await inputsApi.answer('workspace', null, 'input', body);
      } catch (error) {
        expect(await refreshInputAfterConflict(error, refresh)).toBe(true);
      }
      expect(refresh).toHaveBeenCalledTimes(1);
      expect(request).toHaveBeenCalledTimes(1);
      expect(body.answer.text).toBe('Saved draft');
    }
  );
  it('keeps network failures retryable without replacing their idempotency key', async () => {
    const refresh = vi.fn(async () => {});
    expect(
      await refreshInputAfterConflict(new Error('network unavailable'), refresh)
    ).toBe(false);
    expect(refresh).not.toHaveBeenCalled();
  });
  it('closes in one request and appends Undo with the receipt sequence', async () => {
    const request = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            success: true,
            data: { sequence: 8, status: 'closed' },
          })
        )
    );
    setLocalApiTransport({ request, openWebSocket: vi.fn() });
    await inputsApi.status(
      'workspace',
      'remote-host',
      'input',
      7,
      false,
      'close-request'
    );
    expect(request).toHaveBeenLastCalledWith(
      '/api/host/remote-host/workspaces/workspace/inputs/input/close',
      expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({
          expected_sequence: 7,
          request_id: 'close-request',
          status: 'closed',
          reason: null,
        }),
      })
    );
    await inputsApi.status(
      'workspace',
      'remote-host',
      'input',
      8,
      true,
      'undo-request'
    );
    expect(request).toHaveBeenLastCalledWith(
      '/api/host/remote-host/workspaces/workspace/inputs/input/undo-close',
      expect.objectContaining({
        body: JSON.stringify({
          expected_sequence: 8,
          request_id: 'undo-request',
          status: 'closed',
          reason: null,
        }),
      })
    );
  });
  it('keeps conflict errors visible to the status caller', async () => {
    setLocalApiTransport({
      request: vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              success: false,
              message: 'SEQUENCE_CONFLICT: current_sequence=9',
            }),
            { status: 409 }
          )
      ),
      openWebSocket: vi.fn(),
    });
    await expect(
      inputsApi.status('workspace', null, 'input', 7, false, 'request')
    ).rejects.toThrow('SEQUENCE_CONFLICT');
  });
  it('answers only through the UI answer route with the exact version', async () => {
    const request = vi.fn(
      async () => new Response(JSON.stringify({ success: true, data: {} }))
    );
    setLocalApiTransport({ request, openWebSocket: vi.fn() });
    const body = {
      request_id: 'answer',
      expected_sequence: 3,
      version: 2,
      answer: { choice_id: 'ship', text: null },
    };
    await inputsApi.answer('workspace', null, 'input', body);
    expect(request).toHaveBeenCalledWith(
      '/api/workspaces/workspace/inputs/input/answer',
      expect.objectContaining({ body: JSON.stringify(body) })
    );
    expect(
      JSON.parse(String(request.mock.calls[0][1]?.body))
    ).not.toHaveProperty('actor_kind');
  });
});
