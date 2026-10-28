import { afterEach, describe, expect, it, vi } from 'vitest';
import type { InputDetail } from 'shared/types';
import { setLocalApiTransport } from '@/shared/lib/localApiTransport';
import { inputsApi } from './inputsApi';
import { nextOpenInput, splitRecommended } from './inputFlow';

afterEach(() => setLocalApiTransport(null));

const item = (id: string, status: string) =>
  ({ input: { id, status } }) as unknown as InputDetail;

describe('Input flow', () => {
  const list = [
    item('a', 'open'),
    item('b', 'answered'),
    item('c', 'open'),
    item('d', 'open'),
  ];
  it('requests only open Inputs for Pending and keeps answered Inputs in Done', async () => {
    const request = vi.fn(
      async () => new Response(JSON.stringify({ success: true, data: {} }))
    );
    setLocalApiTransport({ request, openWebSocket: vi.fn() });
    await inputsApi.list('workspace', null, false);
    expect(request).toHaveBeenLastCalledWith(
      '/api/workspaces/workspace/inputs?limit=50&status=open&cursor=',
      expect.anything()
    );
    await inputsApi.list('workspace', null, true, 'next');
    expect(request).toHaveBeenLastCalledWith(
      '/api/workspaces/workspace/inputs?limit=50&status=answered,closed,cancelled&cursor=next',
      expect.anything()
    );
  });
  it('advances to the next open Input after the current one', () => {
    expect(nextOpenInput(list, 'a')).toBe('c');
    expect(nextOpenInput(list, 'c')).toBe('d');
  });
  it('wraps to the start of the list', () => {
    expect(nextOpenInput(list, 'd')).toBe('a');
  });
  it('skips the current Input and returns null when none are open', () => {
    expect(
      nextOpenInput([item('a', 'open'), item('b', 'answered')], 'a')
    ).toBeNull();
    expect(nextOpenInput([], 'a')).toBeNull();
  });
  it('starts from the top when the current Input is not listed', () => {
    expect(nextOpenInput(list, 'z')).toBe('a');
  });
  it('marks a recommended choice and strips the suffix', () => {
    expect(splitRecommended('Ship it (recommended)')).toEqual({
      label: 'Ship it',
      recommended: true,
    });
    expect(splitRecommended('Park it')).toEqual({
      label: 'Park it',
      recommended: false,
    });
  });
});
