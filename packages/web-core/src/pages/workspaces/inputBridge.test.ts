import { runInNewContext } from 'node:vm';
import { describe, expect, it, vi } from 'vitest';
import {
  inputEmbeddingDocument,
  inputFrameLoaded,
  createInputBridge,
  readInputProposal,
  validateInputAnswer,
  type InputBridge,
} from './inputBridge';

const contract = {
  choices: [{ id: 'ship', label: 'Ship' }],
  allow_text: true,
  allow_no_answer_yet: false,
};
const makeBridge = (): InputBridge => ({
  id: 'bridge',
  inputId: 'input',
  version: 1,
  loads: 0,
  valid: true,
});
const message = {
  type: 'bc.input.propose',
  protocol: 1,
  bridge_id: 'bridge',
  input_id: 'input',
  version: 1,
  answer: { choice_id: 'ship', text: '<script>plain text</script>' },
};

describe('Input proposal bridge', () => {
  it('issues a new bridge for a trusted retry and rejects the prior page', () => {
    const first = createInputBridge('input', 1);
    const retry = createInputBridge('input', 1);
    expect(retry.id).not.toBe(first.id);
    expect(retry.loads).toBe(0);
    const source = {} as Window;
    expect(
      readInputProposal(
        { source, data: { ...message, bridge_id: first.id } },
        source,
        retry,
        contract
      )
    ).toBeNull();
  });
  it('rejects a forged source even with the correct bridge id', () => {
    const source = {} as Window;
    expect(
      readInputProposal(
        { source: {} as Window, data: message },
        source,
        makeBridge(),
        contract
      )
    ).toBeNull();
  });
  it('rejects a stale bridge id', () => {
    const source = {} as Window;
    expect(
      readInputProposal(
        { source, data: { ...message, bridge_id: 'old' } },
        source,
        makeBridge(),
        contract
      )
    ).toBeNull();
  });
  it('invalidates proposals after an unexpected iframe reload', () => {
    const source = {} as Window;
    const bridge = makeBridge();
    expect(inputFrameLoaded(bridge)).toBe(true);
    expect(inputFrameLoaded(bridge)).toBe(false);
    expect(
      readInputProposal({ source, data: message }, source, bridge, contract)
    ).toBeNull();
  });
  it('accepts only a current validated proposal as plain text data', () => {
    const source = {} as Window;
    expect(
      readInputProposal(
        { source, data: message },
        source,
        makeBridge(),
        contract
      )
    ).toEqual(message.answer);
    expect(
      readInputProposal(
        { source, data: { ...message, version: 2 } },
        source,
        makeBridge(),
        contract
      )
    ).toBeNull();
    expect(
      readInputProposal(
        { source, data: { ...message, answer: { choice_id: 'invented' } } },
        source,
        makeBridge(),
        contract
      )
    ).toBeNull();
  });
  it('rejects unknown fields and oversized answers', () => {
    expect(
      validateInputAnswer(
        { choice_id: 'ship', actor_kind: 'owner_ui' },
        contract
      )
    ).toBe(false);
    expect(
      validateInputAnswer({ text: 'x'.repeat(256 * 1024 + 1) }, contract)
    ).toBe(false);
    expect(validateInputAnswer({}, contract)).toBe(false);
    expect(
      validateInputAnswer({}, { ...contract, allow_no_answer_yet: true })
    ).toBe(true);
  });
  it('delivers the local renderer while retaining the existing proposal handshake', () => {
    vi.stubGlobal('window', {
      location: { href: 'https://app.test/workspaces/w' },
    });
    const document = inputEmbeddingDocument(
      '/api/workspaces/w/inputs/i/versions/1/content',
      makeBridge(),
      'hash'
    );
    const listeners: Record<string, (event?: unknown) => void> = {};
    const sent: unknown[] = [];
    const child = {
      postMessage(data: unknown) {
        sent.push(data);
      },
    };
    const frame = {
      contentWindow: child,
      addEventListener(type: string, fn: () => void) {
        listeners[type] = fn;
      },
    };
    const parent = {
      postMessage(data: unknown) {
        sent.push(data);
      },
    };
    runInNewContext(
      document.match(/<script nonce="[^"]+">([\s\S]*?)<\/script>/)![1],
      {
        document: {
          createElement() {
            return frame;
          },
          body: { append() {} },
        },
        window: {
          addEventListener(type: string, fn: () => void) {
            listeners[type] = fn;
          },
        },
        parent,
      }
    );
    listeners.load();
    expect(sent[0]).toMatchObject({
      type: 'bc.input.init',
      bridge_id: 'bridge',
      input_id: 'input',
      version: 1,
    });
    expect((sent[0] as { mermaid_source?: string }).mermaid_source).toContain(
      'globalThis["mermaid"]'
    );
    listeners.message({ source: child, data: message });
    expect(sent.at(-1)).toEqual(message);
    const image = {
      ...message,
      type: 'bc.input.image.request',
      index: 0,
      src: '.bettercoding-uploads/a.png',
    };
    listeners.message({ source: child, data: image });
    expect(sent.at(-1)).toEqual(image);
    const reply = {
      ...image,
      type: 'bc.input.image',
      src: 'data:image/png;base64,iVBORw==',
    };
    listeners.message({ source: parent, data: reply });
    expect(sent.at(-1)).toEqual(reply);
    const count = sent.length;
    listeners.message({ source: {}, data: message });
    expect(sent).toHaveLength(count);
    vi.unstubAllGlobals();
  });
  it('restricts the embedding frame policy to the exact content route', () => {
    vi.stubGlobal('window', {
      location: { href: 'https://app.test/workspaces/w' },
    });
    const route = '/api/workspaces/w/inputs/i/versions/1/content';
    const document = inputEmbeddingDocument(route, makeBridge(), 'hash');
    expect(document).toContain(`frame-src https://app.test${route};`);
    expect(document).toContain("frame.sandbox='allow-scripts'");
    expect(document).toContain('event.source!==frame.contentWindow');
    expect(document).toContain('loads>1');
    expect(document).toContain('frame.remove()');
    expect(() =>
      inputEmbeddingDocument('https://example.org/escape', makeBridge(), 'hash')
    ).toThrow('Invalid Input content route');
    expect(() =>
      inputEmbeddingDocument('data:text/html,hello', makeBridge(), 'hash')
    ).toThrow('Invalid Input content URL');
    vi.unstubAllGlobals();
  });
});
