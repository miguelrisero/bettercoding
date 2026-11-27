import { beforeEach, describe, expect, it, vi } from 'vitest';

// A minimal hook runtime: Node has no DOM renderer, so the test runs the hook's
// effects the way React does, rerunning an effect only when a dependency changes.
const runtime = vi.hoisted(() => {
  type EffectSlot = { deps: unknown[]; cleanup?: void | (() => void) };
  let slots: unknown[] = [];
  let cursor = 0;
  let pending: (() => void)[] = [];
  let rerender: () => void = () => {};
  return {
    reset(render: () => void) {
      slots = [];
      pending = [];
      rerender = render;
    },
    render(run: () => void) {
      cursor = 0;
      run();
      while (pending.length > 0) pending.shift()!();
    },
    useRef<T>(initial: T) {
      const key = cursor++;
      if (!(key in slots)) slots[key] = { current: initial };
      return slots[key] as { current: T };
    },
    useState<T>(initial: T | (() => T)) {
      const key = cursor++;
      if (!(key in slots)) {
        slots[key] =
          typeof initial === 'function' ? (initial as () => T)() : initial;
      }
      const set = (value: T) => {
        slots[key] = value;
        rerender();
      };
      return [slots[key] as T, set] as const;
    },
    useEffect(effect: () => void | (() => void), deps: unknown[]) {
      const key = cursor++;
      const previous = slots[key] as EffectSlot | undefined;
      if (
        previous &&
        deps.every((dep, index) => Object.is(dep, previous.deps[index]))
      ) {
        return;
      }
      pending.push(() => {
        if (typeof previous?.cleanup === 'function') previous.cleanup();
        slots[key] = { deps, cleanup: effect() };
      });
    },
  };
});

const markSeen = vi.hoisted(() => vi.fn(() => Promise.resolve()));
const queryClient = vi.hoisted(() => ({ invalidateQueries: () => {} }));

vi.mock('react', () => ({
  useEffect: runtime.useEffect,
  useRef: runtime.useRef,
  useState: runtime.useState,
}));
vi.mock('@tanstack/react-query', () => ({
  useQueryClient: () => queryClient,
}));
vi.mock('@/shared/lib/api', () => ({ workspacesApi: { markSeen } }));

import { useMarkWorkspaceSeen } from './useMarkWorkspaceSeen';

const page = {
  visibilityState: 'visible' as DocumentVisibilityState,
  listeners: new Set<() => void>(),
  addEventListener(_type: string, listener: () => void) {
    this.listeners.add(listener);
  },
  removeEventListener(_type: string, listener: () => void) {
    this.listeners.delete(listener);
  },
  setVisibility(state: DocumentVisibilityState) {
    this.visibilityState = state;
    for (const listener of this.listeners) listener();
  },
};
vi.stubGlobal('document', page);

let hasUnseenActivity = false;
const render = () =>
  runtime.render(() =>
    useMarkWorkspaceSeen('workspace', false, hasUnseenActivity)
  );

beforeEach(() => {
  markSeen.mockClear();
  page.visibilityState = 'visible';
  page.listeners.clear();
  hasUnseenActivity = false;
  runtime.reset(render);
});

describe('useMarkWorkspaceSeen', () => {
  it('marks the open workspace seen again when its agent finishes', () => {
    render();
    expect(markSeen).toHaveBeenCalledTimes(1);

    hasUnseenActivity = true;
    render();
    expect(markSeen).toHaveBeenCalledTimes(2);

    hasUnseenActivity = false;
    render();
    expect(markSeen).toHaveBeenCalledTimes(2);
  });

  it('leaves a background tab unread until the user returns', () => {
    render();
    expect(markSeen).toHaveBeenCalledTimes(1);

    page.setVisibility('hidden');
    hasUnseenActivity = true;
    render();
    expect(markSeen).toHaveBeenCalledTimes(1);

    page.setVisibility('visible');
    expect(markSeen).toHaveBeenCalledTimes(2);
  });
});
