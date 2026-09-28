import type { Operation } from 'rfc6902';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// web-core has no DOM test renderer, so `react` is replaced by a minimal
// hook runtime: state and refs by call order, effects re-run when a
// dependency changes, and a state change re-renders synchronously.
const react = vi.hoisted(() => {
  type Effect = { deps?: unknown[]; cleanup?: () => void };
  const slots: unknown[] = [];
  const effects: Effect[] = [];
  let queued: Array<() => void> = [];
  let cursor = 0;
  let effectCursor = 0;
  let rendering = false;
  let dirty = false;
  let component: (() => void) | null = null;

  function render() {
    if (rendering) {
      dirty = true;
      return;
    }
    rendering = true;
    do {
      dirty = false;
      cursor = 0;
      effectCursor = 0;
      component?.();
      const pending = queued;
      queued = [];
      pending.forEach((run) => run());
    } while (dirty);
    rendering = false;
  }

  return {
    mount(next: () => void) {
      component = next;
      render();
    },
    unmount() {
      effects.forEach((effect) => effect.cleanup?.());
      slots.length = 0;
      effects.length = 0;
      queued = [];
      rendering = false;
      dirty = false;
      component = null;
    },
    useRef<T>(initial: T) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = { current: initial };
      return slots[index] as { current: T };
    },
    useState<T>(initial: T | (() => T)) {
      const index = cursor++;
      if (!(index in slots)) {
        slots[index] =
          typeof initial === 'function' ? (initial as () => T)() : initial;
      }
      const set = (value: T | ((previous: T) => T)) => {
        const previous = slots[index] as T;
        const next =
          typeof value === 'function'
            ? (value as (previous: T) => T)(previous)
            : value;
        if (Object.is(previous, next)) return;
        slots[index] = next;
        render();
      };
      return [slots[index] as T, set] as const;
    },
    useEffect(run: () => void | (() => void), deps?: unknown[]) {
      const index = effectCursor++;
      const previous = effects[index];
      const changed =
        !previous ||
        !deps ||
        deps.some((dep, i) => !Object.is(dep, previous.deps?.[i]));
      if (!changed) return;
      queued.push(() => {
        previous?.cleanup?.();
        effects[index] = { deps, cleanup: run() ?? undefined };
      });
    },
  };
});

class FakeSocket {
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: ((event: { code: number; wasClean: boolean }) => void) | null = null;
  // Like a browser socket, closing fires `onclose` on a later task.
  close = vi.fn(() => {
    setTimeout(() => this.onclose?.({ code: 1005, wasClean: true }), 0);
  });

  receive(message: unknown) {
    this.onmessage?.({ data: JSON.stringify(message) });
  }
}

const sockets = vi.hoisted(() => [] as FakeSocket[]);

vi.mock('react', () => ({
  useEffect: react.useEffect,
  useRef: react.useRef,
  useState: react.useState,
}));

vi.mock('@/shared/lib/localApiTransport', () => ({
  openLocalApiWebSocket: vi.fn(async () => {
    const socket = new FakeSocket();
    sockets.push(socket);
    return socket;
  }),
}));

type Feed = { entries: string[] };

const initialData = (): Feed => ({ entries: [] });
const snapshot = (entries: string[]) => ({
  JsonPatch: [
    { op: 'replace', path: '/entries', value: entries },
  ] satisfies Operation[],
});
const append = (index: number, value: string) => ({
  JsonPatch: [
    { op: 'add', path: `/entries/${index}`, value },
  ] satisfies Operation[],
});

async function mountStream() {
  const { useJsonPatchWsStream } = await import('./useJsonPatchWsStream');
  let result!: ReturnType<typeof useJsonPatchWsStream<Feed>>;
  react.mount(() => {
    result = useJsonPatchWsStream<Feed>('/api/feed', true, initialData, {
      resyncOnPatchFailure: true,
    });
  });
  // Let the socket open resolve and attach its handlers.
  await vi.advanceTimersByTimeAsync(0);
  return () => result;
}

/** Deliver a snapshot on the newest socket and mark it ready. */
function connectWith(entries: string[]) {
  const socket = sockets[sockets.length - 1];
  socket.onopen?.();
  socket.receive(snapshot(entries));
  socket.receive({ Ready: true });
  return socket;
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal('window', globalThis);
  vi.spyOn(console, 'warn').mockImplementation(() => {});
  sockets.length = 0;
});

afterEach(() => {
  react.unmount();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('useJsonPatchWsStream resync on a failed patch', () => {
  it('drops the socket and loads a fresh snapshot from a new one', async () => {
    const stream = await mountStream();
    const first = connectWith(['a']);
    expect(stream().data).toEqual({ entries: ['a'] });
    expect(stream().isConnected).toBe(true);

    // An add past the end of the array means the client drifted.
    first.receive(append(5, 'x'));
    expect(first.close).toHaveBeenCalledTimes(1);
    expect(first.onclose).toBeNull();
    expect(stream().isConnected).toBe(false);
    expect(sockets).toHaveLength(1);

    // The first resync is immediate.
    await vi.advanceTimersByTimeAsync(0);
    expect(sockets).toHaveLength(2);
    expect(stream().data).toBeUndefined();

    const second = connectWith(['a', 'b']);
    expect(stream().data).toEqual({ entries: ['a', 'b'] });
    expect(stream().isConnected).toBe(true);
    expect(stream().isInitialized).toBe(true);

    // The dropped socket's close does not schedule a second reconnect that
    // would replace the new socket.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(sockets).toHaveLength(2);
    expect(second.close).not.toHaveBeenCalled();

    react.unmount();
    expect(second.close).toHaveBeenCalledTimes(1);
  });

  it('backs off repeated resyncs until a live update applies', async () => {
    await mountStream();
    connectWith(['a']).receive(append(5, 'x'));
    await vi.advanceTimersByTimeAsync(0);
    expect(sockets).toHaveLength(2);

    // A snapshot alone does not prove the resync worked: the next failure
    // waits 2 s.
    connectWith(['a']).receive(append(5, 'x'));
    await vi.advanceTimersByTimeAsync(1999);
    expect(sockets).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(sockets).toHaveLength(3);

    // A live update that applies resets the backoff.
    const third = connectWith(['a']);
    third.receive(append(1, 'b'));
    third.receive(append(5, 'x'));
    await vi.advanceTimersByTimeAsync(0);
    expect(sockets).toHaveLength(4);
  });
});
