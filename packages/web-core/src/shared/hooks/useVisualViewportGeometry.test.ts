import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

type CapturedListener = EventListenerOrEventListenerObject;

const subscriptions: Array<() => void> = [];

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

async function createViewportHarness({
  touch = true,
  autoSubscribe = true,
} = {}) {
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
  vi.stubGlobal('navigator', { maxTouchPoints: touch ? 1 : 0 });
  vi.stubGlobal(
    'requestAnimationFrame',
    (callback: FrameRequestCallback): number =>
      setTimeout(() => callback(0), 0) as unknown as number
  );

  const { visualViewportStore: store } = await import(
    './useVisualViewportGeometry'
  );
  const listener = vi.fn();

  const subscribe = (nextListener: () => void) => {
    const unsubscribe = store.subscribe(nextListener);
    subscriptions.push(unsubscribe);
    return unsubscribe;
  };

  if (autoSubscribe) subscribe(listener);

  return {
    visualViewport,
    scrollTo,
    store,
    listener,
    subscribe,
    viewportAddEventListener: viewportEvents.addEventListener,
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
    subscriptions.length = 0;
  });

  afterEach(() => {
    for (const unsubscribe of subscriptions.splice(0)) unsubscribe();
    vi.clearAllTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('stores residual iOS visual viewport pan without mutating scroll', async () => {
    const harness = await createViewportHarness();
    harness.visualViewport.height = 498.9;
    harness.visualViewport.offsetTop = 150.9;

    harness.dispatchViewport('scroll');

    expect(harness.scrollTo).not.toHaveBeenCalled();
    expect(harness.store.getSnapshot()).toEqual({
      height: 498,
      offsetTop: 150,
    });
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
  });

  it('replaces pending settle checks instead of accumulating timeouts', async () => {
    const harness = await createViewportHarness();
    await vi.advanceTimersByTimeAsync(0);

    harness.dispatchWindow('focusout');
    harness.dispatchWindow('focusout');
    harness.dispatchWindow('focusout');

    expect(vi.getTimerCount()).toBe(2);
  });

  it('keeps fallback snapshot identity stable through pinch zoom', async () => {
    const harness = await createViewportHarness();
    harness.visualViewport.height = 422;
    harness.visualViewport.offsetTop = 90;
    harness.visualViewport.scale = 2;

    harness.dispatchViewport('resize');
    const pinchSnapshot = harness.store.getSnapshot();
    harness.dispatchViewport('scroll');

    expect(pinchSnapshot).toEqual({ height: null, offsetTop: 0 });
    expect(harness.store.getSnapshot()).toBe(pinchSnapshot);
    expect(harness.scrollTo).not.toHaveBeenCalled();
  });

  it('keeps fallback snapshot identity stable on non-touch devices', async () => {
    const harness = await createViewportHarness({ touch: false });
    const firstSnapshot = harness.store.getSnapshot();

    expect(firstSnapshot).toEqual({ height: null, offsetTop: 0 });
    expect(harness.store.getSnapshot()).toBe(firstSnapshot);
    expect(harness.viewportAddEventListener).not.toHaveBeenCalled();
  });

  it('keeps snapshot identity stable across no-op events', async () => {
    const harness = await createViewportHarness();
    const before = harness.store.getSnapshot();

    harness.dispatchViewport('resize');

    expect(harness.store.getSnapshot()).toBe(before);
  });

  it('notifies every subscriber when an earlier subscriber throws', async () => {
    const harness = await createViewportHarness({ autoSubscribe: false });
    const throwingListener = vi.fn(() => {
      throw new Error('subscriber failed');
    });
    const healthyListener = vi.fn();
    harness.subscribe(throwingListener);
    harness.subscribe(healthyListener);

    await vi.advanceTimersByTimeAsync(0);

    expect(throwingListener).toHaveBeenCalledOnce();
    expect(healthyListener).toHaveBeenCalledOnce();
  });
});
