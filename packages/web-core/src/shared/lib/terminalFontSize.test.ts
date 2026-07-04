import { describe, it, expect } from 'vitest';

import {
  clampTerminalFontSize,
  TERMINAL_DEFAULT_FONT_SIZE,
  TERMINAL_MAX_FONT_SIZE,
  TERMINAL_MIN_FONT_SIZE,
} from './terminalFontSize';

describe('clampTerminalFontSize', () => {
  it('keeps values inside the range', () => {
    expect(clampTerminalFontSize(12)).toBe(12);
    expect(clampTerminalFontSize(TERMINAL_MIN_FONT_SIZE)).toBe(
      TERMINAL_MIN_FONT_SIZE
    );
    expect(clampTerminalFontSize(TERMINAL_MAX_FONT_SIZE)).toBe(
      TERMINAL_MAX_FONT_SIZE
    );
  });

  it('clamps out-of-range values', () => {
    expect(clampTerminalFontSize(2)).toBe(TERMINAL_MIN_FONT_SIZE);
    expect(clampTerminalFontSize(99)).toBe(TERMINAL_MAX_FONT_SIZE);
  });

  it('rounds and falls back on garbage', () => {
    expect(clampTerminalFontSize(13.6)).toBe(14);
    expect(clampTerminalFontSize(NaN)).toBe(TERMINAL_DEFAULT_FONT_SIZE);
    expect(clampTerminalFontSize(Infinity)).toBe(TERMINAL_DEFAULT_FONT_SIZE);
  });
});
