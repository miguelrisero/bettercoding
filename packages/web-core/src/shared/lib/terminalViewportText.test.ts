import { describe, it, expect } from 'vitest';

import { extractViewportText } from './terminalViewportText';

type FakeLine = {
  isWrapped: boolean;
  translateToString: (trimRight?: boolean) => string;
};

function line(text: string, isWrapped = false): FakeLine {
  return {
    isWrapped,
    translateToString: (trimRight?: boolean) =>
      trimRight ? text.replace(/\s+$/, '') : text,
  };
}

function buffer(lines: Array<FakeLine | undefined>) {
  return { getLine: (i: number) => lines[i] } as never;
}

describe('extractViewportText', () => {
  it('joins separate logical lines with newlines', () => {
    const buf = buffer([line('foo'), line('bar')]);
    expect(extractViewportText(buf, 0, 2)).toBe('foo\nbar');
  });

  it('keeps a wrapped line whole (no newline at the wrap point)', () => {
    // Row 1 is a continuation of row 0 (isWrapped === true).
    const buf = buffer([line('AAAA'), line('BBBB', true)]);
    expect(extractViewportText(buf, 0, 2)).toBe('AAAABBBB');
  });

  it('preserves trailing spaces within a wrapped segment but trims the tail row', () => {
    const buf = buffer([line('one '), line('two', true), line('three')]);
    // "one " keeps its space (continued), "two" then ends the logical line.
    expect(extractViewportText(buf, 0, 3)).toBe('one two\nthree');
  });

  it('trims trailing blank rows', () => {
    const buf = buffer([line('x'), line('   '), line('')]);
    expect(extractViewportText(buf, 0, 3)).toBe('x');
  });

  it('tolerates missing rows without throwing', () => {
    const buf = buffer([line('a'), undefined, line('b')]);
    expect(extractViewportText(buf, 0, 3)).toBe('a\nb');
  });

  it('honors the viewport offset', () => {
    const buf = buffer([line('hidden'), line('shown')]);
    expect(extractViewportText(buf, 1, 1)).toBe('shown');
  });
});
