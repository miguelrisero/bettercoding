import { runInNewContext } from 'node:vm';
import type { InputDetail } from 'shared/types';
import { afterEach, expect, it, vi } from 'vitest';
import type { LocalApiTransport } from '@/shared/lib/localApiTransport';

function publication(version: number): InputDetail {
  return {
    input: {
      id: 'input',
      workspace_id: 'workspace',
      external_key: 'synthetic',
      creator: 'synthetic',
      current_version: version,
      status: 'open',
      priority: 'P2',
      sequence: version,
      show_at: null,
      created_at: '2026-11-01T00:00:00Z',
      updated_at: '2026-11-01T00:00:00Z',
    },
    version: {
      version,
      title: `Publication ${version}`,
      html: null,
      content_hash: `hash-${version}`,
      contract: {
        choices: [{ id: `choice-${version}`, label: `Choice ${version}` }],
        allow_text: true,
        allow_no_answer_yet: false,
      },
    },
    answers: [],
  };
}

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it('refreshes an open detail on the 10-second cadence and retains stable version dependencies', async () => {
  // QueryObserver enables interval timers only in a browser environment.
  vi.stubGlobal('window', {
    location: { href: 'https://app.test/workspaces/workspace' },
  });
  vi.useFakeTimers();
  const { QueryClient, QueryObserver } = await import('@tanstack/react-query');
  const { queryClient } = await import('@/shared/lib/queryClient');
  const { setLocalApiTransport } = await import(
    '@/shared/lib/localApiTransport'
  );
  const { inputDetailQueryOptions, inputsApi } = await import('./inputsApi');
  const { createInputBridge, inputEmbeddingDocument, readInputProposal } =
    await import('./inputBridge');
  let published = publication(1);
  const request = vi.fn<LocalApiTransport['request']>(
    async () => new Response(JSON.stringify({ success: true, data: published }))
  );
  setLocalApiTransport({ request, openWebSocket: vi.fn() });
  const client = new QueryClient({
    defaultOptions: queryClient.getDefaultOptions(),
  });
  const observer = new QueryObserver(
    client,
    inputDetailQueryOptions('workspace', 'remote-host', 'input')
  );
  const unsubscribe = observer.subscribe(() => {});
  try {
    await vi.advanceTimersByTimeAsync(0);
    const initial = observer.getCurrentResult().data!;
    expect(initial.version.title).toBe('Publication 1');

    // An unchanged poll retains the objects used by the body's key and effects.
    await vi.advanceTimersByTimeAsync(10000);
    expect(observer.getCurrentResult().data).toBe(initial);
    published = { ...published, input: { ...published.input, sequence: 2 } };
    await vi.advanceTimersByTimeAsync(10000);
    expect(observer.getCurrentResult().data!.version).toBe(initial.version);
    expect(observer.getCurrentResult().data!.input.sequence).toBe(2);

    published = publication(2);
    published.input.sequence = 3;
    await vi.advanceTimersByTimeAsync(9999);
    expect(observer.getCurrentResult().data!.version.title).toBe(
      'Publication 1'
    );
    await vi.advanceTimersByTimeAsync(1);
    const current = observer.getCurrentResult().data!;
    expect(current.version.title).toBe('Publication 2');
    expect(current.version.version).toBe(2);
    expect(current.version.contract.choices).toEqual([
      { id: 'choice-2', label: 'Choice 2' },
    ]);
    const url = await inputsApi.contentUrl('workspace', 'remote-host', current);
    expect(url).toBe(
      '/api/host/remote-host/workspaces/workspace/inputs/input/versions/2/content'
    );

    const bridge = createInputBridge(current.input.id, current.version.version);
    const document = inputEmbeddingDocument(
      url,
      bridge,
      current.version.content_hash
    );
    let load!: () => void;
    const sent: unknown[] = [];
    const source = { postMessage: (data: unknown) => sent.push(data) };
    runInNewContext(
      document.match(/<script nonce="[^"]+">([\s\S]*?)<\/script>/)![1],
      {
        document: {
          createElement: () => ({
            contentWindow: source,
            addEventListener: (_type: string, listener: () => void) => {
              load = listener;
            },
          }),
          body: { append() {} },
        },
        window: { addEventListener() {} },
        parent: { postMessage() {} },
      }
    );
    load();
    expect(sent[0]).toMatchObject({
      input_id: 'input',
      version: 2,
      content_hash: 'hash-2',
      bridge_id: bridge.id,
    });
    const draft = { choice_id: 'choice-2', text: 'Saved draft' };
    const proposal = {
      type: 'bc.input.propose',
      protocol: 1,
      bridge_id: bridge.id,
      input_id: 'input',
      version: 2,
      answer: draft,
    };
    const frameWindow = source as unknown as Window;
    expect(
      readInputProposal(
        { source: frameWindow, data: { ...proposal, version: 1 } },
        frameWindow,
        bridge,
        current.version.contract
      )
    ).toBeNull();
    expect(
      readInputProposal(
        { source: frameWindow, data: proposal },
        frameWindow,
        bridge,
        current.version.contract
      )
    ).toEqual(draft);
    expect(request.mock.calls.every(([, init]) => init?.method === 'GET')).toBe(
      true
    );

    // Only an explicit answer action sends a write using the selected version.
    await inputsApi.answer('workspace', 'remote-host', current.input.id, {
      request_id: 'explicit-submit',
      expected_sequence: current.input.sequence,
      version: current.version.version,
      answer: draft,
    });
    expect(request).toHaveBeenLastCalledWith(
      '/api/host/remote-host/workspaces/workspace/inputs/input/answer',
      expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({
          request_id: 'explicit-submit',
          expected_sequence: 3,
          version: 2,
          answer: draft,
        }),
      })
    );
  } finally {
    unsubscribe();
    client.clear();
    setLocalApiTransport(null);
  }
});
