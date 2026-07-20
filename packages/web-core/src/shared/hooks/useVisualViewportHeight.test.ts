import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

type CapturedListener = EventListenerOrEventListenerObject;

function createEventTargetStub() {
  const listeners = new Map<string, Set<CapturedListener>>();

  return {
    addEventListener: vi.fn((type: string, listener: CapturedListener) => {
      const captured = listeners.get(type) ?? new Set<CapturedListener>();
      captured.add(listener);
      listeners.set(type, captured);
    }),
    dispatchEvent(event: Event) {
      for (const listener of listeners.get(event.type) ?? []) {
        if (typeof listener === 'function') {
          listener(event);
        } else {
          listener.handleEvent(event);
        }
      }
      return true;
    },
  };
}

async function createViewportHarness() {
  const viewportEvents = createEventTargetStub();
  const windowEvents = createEventTargetStub();
  const documentEvents = createEventTargetStub();
  const visualViewport = {
    height: 844,
    offsetTop: 0,
    scale: 1,
    addEventListener: viewportEvents.addEventListener,
  };
  const scrollTo = vi.fn();
  const fakeWindow = {
    visualViewport,
    scrollTo,
    matchMedia: vi.fn(() => ({ matches: false })),
    addEventListener: windowEvents.addEventListener,
  };
  const fakeDocument = {
    addEventListener: documentEvents.addEventListener,
  };

  vi.stubGlobal('window', fakeWindow);
  vi.stubGlobal('document', fakeDocument);
  vi.stubGlobal('navigator', { maxTouchPoints: 1 });
  vi.stubGlobal(
    'requestAnimationFrame',
    (callback: FrameRequestCallback): number =>
      setTimeout(() => callback(0), 0) as unknown as number
  );

  const { __vvStoreForTests: store } = await import(
    './useVisualViewportHeight'
  );
  const unsubscribe = store.subscribe(vi.fn());

  return {
    visualViewport,
    scrollTo,
    store,
    unsubscribe,
    dispatchViewport(type: string) {
      viewportEvents.dispatchEvent(new Event(type));
    },
    dispatchWindow(type: string) {
      windowEvents.dispatchEvent(new Event(type));
    },
  };
}

describe('visual viewport geometry store', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.resetModules();
  });

  afterEach(() => {
    vi.clearAllTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('stores residual iOS visual viewport pan after scroll reset', async () => {
    const harness = await createViewportHarness();
    harness.visualViewport.height = 498.9;
    harness.visualViewport.offsetTop = 150.9;

    harness.dispatchViewport('scroll');

    expect(harness.scrollTo).toHaveBeenCalledWith(0, 0);
    expect(harness.store.getSnapshot()).toEqual({
      height: 498,
      offsetTop: 150,
    });
    harness.unsubscribe();
  });

  it('recovers a silently restored height after focus settles', async () => {
    const harness = await createViewportHarness();
    harness.visualViewport.height = 498;
    harness.dispatchViewport('resize');
    expect(harness.store.getSnapshot().height).toBe(498);

    harness.visualViewport.height = 844;
    harness.dispatchWindow('focusout');
    await vi.advanceTimersByTimeAsync(700);

    expect(harness.store.getSnapshot().height).toBe(844);
    harness.unsubscribe();
  });

  it('falls back to layout geometry during pinch zoom', async () => {
    const harness = await createViewportHarness();
    harness.visualViewport.height = 422;
    harness.visualViewport.offsetTop = 90;
    harness.visualViewport.scale = 2;

    harness.dispatchViewport('resize');

    expect(harness.store.getSnapshot()).toEqual({
      height: null,
      offsetTop: 0,
    });
    expect(harness.scrollTo).not.toHaveBeenCalled();
    harness.unsubscribe();
  });

  it('keeps snapshot identity stable across no-op events', async () => {
    const harness = await createViewportHarness();
    const before = harness.store.getSnapshot();

    harness.dispatchViewport('resize');

    expect(harness.store.getSnapshot()).toBe(before);
    harness.unsubscribe();
  });
});
