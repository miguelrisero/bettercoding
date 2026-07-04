import { describe, it, expect } from 'vitest';

import {
  applyStickyCtrl,
  keySequence,
  toCtrlChar,
} from './terminalKeySequences';

describe('keySequence', () => {
  it('returns fixed sequences for non-arrow keys regardless of cursor mode', () => {
    for (const app of [false, true]) {
      expect(keySequence('esc', app)).toBe('\x1b');
      expect(keySequence('tab', app)).toBe('\t');
      expect(keySequence('shift-tab', app)).toBe('\x1b[Z');
      expect(keySequence('ctrl-c', app)).toBe('\x03');
      expect(keySequence('enter', app)).toBe('\r');
    }
  });

  it('uses CSI arrows in normal cursor mode', () => {
    expect(keySequence('up', false)).toBe('\x1b[A');
    expect(keySequence('down', false)).toBe('\x1b[B');
    expect(keySequence('right', false)).toBe('\x1b[C');
    expect(keySequence('left', false)).toBe('\x1b[D');
  });

  it('uses SS3 arrows in application cursor mode (claude/tmux)', () => {
    expect(keySequence('up', true)).toBe('\x1bOA');
    expect(keySequence('down', true)).toBe('\x1bOB');
    expect(keySequence('right', true)).toBe('\x1bOC');
    expect(keySequence('left', true)).toBe('\x1bOD');
  });
});

describe('toCtrlChar', () => {
  it('maps letters case-insensitively to control codes', () => {
    expect(toCtrlChar('c')).toBe('\x03');
    expect(toCtrlChar('C')).toBe('\x03');
    expect(toCtrlChar('a')).toBe('\x01');
    expect(toCtrlChar('z')).toBe('\x1a');
  });

  it('maps the classic punctuation combos', () => {
    expect(toCtrlChar('@')).toBe('\x00');
    expect(toCtrlChar('[')).toBe('\x1b');
    expect(toCtrlChar('\\')).toBe('\x1c');
    expect(toCtrlChar(']')).toBe('\x1d');
    expect(toCtrlChar('^')).toBe('\x1e');
    expect(toCtrlChar('_')).toBe('\x1f');
    expect(toCtrlChar(' ')).toBe('\x00');
    expect(toCtrlChar('?')).toBe('\x7f');
  });

  it('returns null for keys without a control code', () => {
    expect(toCtrlChar('1')).toBeNull();
    expect(toCtrlChar('.')).toBeNull();
    expect(toCtrlChar('ab')).toBeNull();
    expect(toCtrlChar('')).toBeNull();
  });
});

describe('applyStickyCtrl', () => {
  it('transforms a single mappable character', () => {
    expect(applyStickyCtrl('c')).toEqual({ out: '\x03', applied: true });
    expect(applyStickyCtrl('D')).toEqual({ out: '\x04', applied: true });
  });

  it('passes through single unmappable characters', () => {
    expect(applyStickyCtrl('1')).toEqual({ out: '1', applied: false });
  });

  it('passes through multi-char bursts (paste, IME, escape sequences)', () => {
    expect(applyStickyCtrl('hello')).toEqual({ out: 'hello', applied: false });
    expect(applyStickyCtrl('\x1b[A')).toEqual({
      out: '\x1b[A',
      applied: false,
    });
  });
});
