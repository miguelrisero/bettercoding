import { describe, it, expect } from 'vitest';

import { linearSelection, touchToCell } from './terminalTouchSelection';

const rect = { left: 10, top: 20, width: 800, height: 480 };
const cols = 80; // cell width 10
const rows = 24; // cell height 20

describe('touchToCell', () => {
  it('maps a touch point to its cell', () => {
    expect(touchToCell(10, 20, rect, cols, rows)).toEqual({ col: 0, row: 0 });
    expect(touchToCell(25, 45, rect, cols, rows)).toEqual({ col: 1, row: 1 });
    expect(touchToCell(809, 499, rect, cols, rows)).toEqual({
      col: 79,
      row: 23,
    });
  });

  it('clamps points outside the screen rect', () => {
    expect(touchToCell(0, 0, rect, cols, rows)).toEqual({ col: 0, row: 0 });
    expect(touchToCell(2000, 2000, rect, cols, rows)).toEqual({
      col: 79,
      row: 23,
    });
  });
});

describe('linearSelection', () => {
  it('selects forward within a line (inclusive)', () => {
    expect(
      linearSelection({ col: 2, row: 5 }, { col: 6, row: 5 }, cols)
    ).toEqual({ col: 2, row: 5, length: 5 });
  });

  it('normalizes a backwards drag', () => {
    expect(
      linearSelection({ col: 6, row: 5 }, { col: 2, row: 5 }, cols)
    ).toEqual({ col: 2, row: 5, length: 5 });
  });

  it('spans lines', () => {
    expect(
      linearSelection({ col: 78, row: 3 }, { col: 1, row: 4 }, cols)
    ).toEqual({ col: 78, row: 3, length: 4 });
  });

  it('a single cell has length 1', () => {
    expect(
      linearSelection({ col: 4, row: 2 }, { col: 4, row: 2 }, cols)
    ).toEqual({ col: 4, row: 2, length: 1 });
  });
});

describe('touchToCell — degenerate rects (review round 1)', () => {
  it('returns null instead of NaN cells for zero-sized rects', () => {
    const zero = { left: 0, top: 0, width: 0, height: 0 };
    expect(touchToCell(0, 0, zero, cols, rows)).toBeNull();
    expect(touchToCell(10, 10, { ...rect, width: 0 }, cols, rows)).toBeNull();
    expect(touchToCell(10, 10, { ...rect, height: 0 }, cols, rows)).toBeNull();
    expect(touchToCell(10, 10, rect, 0, rows)).toBeNull();
  });
});
