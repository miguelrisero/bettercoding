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
