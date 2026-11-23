import { describe, expect, it } from 'vitest';
import type { InputAnswer, InputDetail } from 'shared/types';
import { TAP_SLOP_PX } from '@/shared/lib/terminalTouchGestures';
import { validateInputAnswer } from './inputBridge';
import {
  availablePriorInputDraft,
  compareInputs,
  consumeInputDraft,
  nextOpenInput,
  readInputDraft,
  splitRecommended,
  inputNavigationIsCurrent,
  inputSwipeStep,
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
  it('moves in both swipe directions and ignores taps and vertical gestures', () => {
    expect(inputSwipeStep(-48, 5)).toBe(1);
    expect(inputSwipeStep(48, 5)).toBe(-1);
    expect(inputSwipeStep(TAP_SLOP_PX, 0)).toBeNull();
    expect(inputSwipeStep(30, 60)).toBeNull();
    expect(inputSwipeStep(30, 30)).toBeNull();
  });
  it('restores the old follow-up message into the reply without dropping the choice', () => {
    const storage = new Map([
      ['draft', JSON.stringify({ choice_id: 'ship', text: null })],
      ['follow-up', 'Saved message'],
    ]);
    const reader = { getItem: (key: string) => storage.get(key) ?? null };
    expect(readInputDraft(reader, 'draft', 'follow-up', true)).toEqual({
      choice_id: 'ship',
      text: 'Saved message',
    });
    expect(readInputDraft(reader, 'draft', 'follow-up', false)).toEqual({
      choice_id: 'ship',
      text: null,
    });
    storage.set(
      'draft',
      JSON.stringify({ choice_id: 'ship', text: 'Saved note' })
    );
    expect(readInputDraft(reader, 'draft', 'follow-up', true)).toEqual({
      choice_id: 'ship',
      text: 'Saved note',
    });
  });
  it('hides a prior draft during its write and discards the next version’s consumed copy', () => {
    const prior = {
      key: 'version-1',
      answer: { choice_id: 'ship', text: 'More context' },
    };
    const drafts = new Map<string, InputAnswer>([[prior.key, prior.answer]]);

    // Version 2 can load its copy before version 1 receives the answer receipt.
    expect(availablePriorInputDraft(prior, drafts, true)).toBeNull();
    expect(availablePriorInputDraft(prior, drafts, false)).toEqual(
      prior.answer
    );

    // The receipt clears the original draft after version 2 has taken its copy.
    consumeInputDraft(drafts, prior.key, prior.answer);
    expect(availablePriorInputDraft(prior, drafts, false)).toBeNull();

    const edited = { choice_id: 'ship', text: 'New context' };
    drafts.set(prior.key, edited);
    expect(consumeInputDraft(drafts, prior.key, prior.answer)).toBeNull();
    expect(availablePriorInputDraft(prior, drafts, false)).toEqual(edited);
  });
  it('accepts a reply in a legacy choice-only contract and preserves deferral rules', () => {
    const contract = {
      choices: [{ id: 'ship', label: 'Ship' }],
      allow_text: false,
      allow_no_answer_yet: false,
    };
    expect(validateInputAnswer({ text: 'More context' }, contract)).toBe(true);
    expect(
      validateInputAnswer({ choice_id: 'ship', text: 'Go' }, contract)
    ).toBe(true);
    expect(
      validateInputAnswer(
        { choice_id: 'other', text: 'More context' },
        contract
      )
    ).toBe(false);
    expect(validateInputAnswer({ text: ' ' }, contract)).toBe(false);
    expect(
      validateInputAnswer(
        { text: ' ' },
        { ...contract, allow_no_answer_yet: true }
      )
    ).toBe(true);
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
