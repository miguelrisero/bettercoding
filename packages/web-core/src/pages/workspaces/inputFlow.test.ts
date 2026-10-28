import { describe, expect, it } from 'vitest';
import type { InputDetail } from 'shared/types';
import { nextOpenInput, splitRecommended } from './inputFlow';

const item = (id: string, status: string) =>
  ({ input: { id, status } }) as unknown as InputDetail;

describe('Input flow', () => {
  const list = [
    item('a', 'open'),
    item('b', 'answered'),
    item('c', 'open'),
    item('d', 'open'),
  ];
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
