import { describe, expect, it } from 'vitest';
import type { InputDetail } from 'shared/types';
import {
  compareInputs,
  nextOpenInput,
  splitRecommended,
  inputNavigationIsCurrent,
} from './inputFlow';

const item = (
  id: string,
  status: string,
  priority = 'P2',
  created_at = '2026-10-28T00:00:00Z'
) =>
  ({ input: { id, status, priority, created_at } }) as unknown as InputDetail;

describe('Input flow', () => {
  const list = [
    item('a', 'open'),
    item('b', 'answered'),
    item('c', 'open'),
    item('d', 'open'),
  ];
  it('guards a late schedule or answer against closing, moving, or reopening', () => {
    expect(inputNavigationIsCurrent(3, 3, 'a', 'a')).toBe(true);
    expect(inputNavigationIsCurrent(3, 4, 'a', 'a')).toBe(false);
    expect(inputNavigationIsCurrent(3, 3, 'a', 'b')).toBe(false);
    expect(inputNavigationIsCurrent(3, 4, 'a', null)).toBe(false);
  });
  it('advances to the next open Input after the current one', () => {
    expect(nextOpenInput(list, item('a', 'answered'), 'priority')).toBe('c');
    expect(nextOpenInput(list, item('c', 'answered'), 'priority')).toBe('d');
  });
  it('wraps to the start of the list', () => {
    expect(nextOpenInput(list, item('d', 'answered'), 'priority')).toBe('a');
  });
  it('skips the current Input and returns null when none are open', () => {
    expect(
      nextOpenInput(
        [item('a', 'open'), item('b', 'answered')],
        item('a', 'open'),
        'priority'
      )
    ).toBeNull();
    expect(nextOpenInput([], item('a', 'open'), 'priority')).toBeNull();
  });
  it('uses the id tie-break after the answered Input leaves the pending list', () => {
    const pending = [item('a', 'open'), item('c', 'open')];
    expect(nextOpenInput(pending, item('b', 'answered'), 'priority')).toBe('c');
    expect(nextOpenInput(pending, item('z', 'answered'), 'priority')).toBe('a');
  });
  it('sorts by priority, then oldest age, then id', () => {
    const inputs = [
      item('c', 'open'),
      item('b', 'open', 'P2', '2026-10-27T00:00:00Z'),
      item('z', 'open', 'P0'),
      item('a', 'open'),
    ];
    expect(
      inputs
        .sort((a, b) => compareInputs(a, b, 'priority'))
        .map((d) => d.input.id)
    ).toEqual(['z', 'b', 'a', 'c']);
  });
  it('sorts newest first with an id tie-break', () => {
    const inputs = [
      item('c', 'open', 'P0', '2026-10-27T00:00:00Z'),
      item('b', 'open'),
      item('a', 'open', 'P4'),
    ];
    expect(
      inputs
        .sort((a, b) => compareInputs(a, b, 'newest'))
        .map((d) => d.input.id)
    ).toEqual(['a', 'b', 'c']);
  });
  it('places an absent answered detail in priority order and wraps', () => {
    const pending = [item('a', 'open', 'P4'), item('z', 'open', 'P0')];
    expect(
      nextOpenInput(pending, item('b', 'answered', 'P1'), 'priority')
    ).toBe('a');
    expect(
      nextOpenInput(pending, item('b', 'answered', 'P4'), 'priority')
    ).toBe('z');
    expect(
      nextOpenInput(
        pending,
        item('b', 'answered', 'P1', '2026-10-27T00:00:00Z'),
        'newest'
      )
    ).toBe('a');
  });
  it('uses refreshed priority when the current Input stays open', () => {
    const pending = [
      item('a', 'open', 'P0'),
      item('b', 'open', 'P1'),
      item('c', 'open', 'P2'),
    ];
    expect(nextOpenInput(pending, item('b', 'open', 'P4'), 'priority')).toBe(
      'c'
    );
  });
  it('uses newest order after an answered Input leaves Pending', () => {
    const pending = [
      item('a', 'open', 'P0', '2026-10-26T00:00:00Z'),
      item('z', 'open', 'P4', '2026-10-28T00:00:00Z'),
    ];
    expect(
      nextOpenInput(
        pending,
        item('b', 'answered', 'P1', '2026-10-27T00:00:00Z'),
        'newest'
      )
    ).toBe('a');
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
