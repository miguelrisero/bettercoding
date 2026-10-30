import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  clampSidebarWidth,
  readSidebarWidth,
  writeSidebarWidth,
} from './workspaceSidebarWidths';

describe('workspace sidebar pixel preferences', () => {
  beforeEach(() => {
    const values = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
    });
  });
  afterEach(() => vi.unstubAllGlobals());

  it.each([
    [100, 200],
    [-20, 200],
    [900, 720],
    [420, 420],
    [undefined, 300],
    [null, 300],
    ['', 300],
    ['420', 300],
    [NaN, 300],
    [Infinity, 300],
    [-Infinity, 300],
  ])('bounds the width %s to %s pixels', (value, expected) => {
    expect(clampSidebarWidth(value)).toBe(expected);
  });

  it('restores independent widths and a reset through fresh reads', () => {
    expect(readSidebarWidth('left')).toBe(300);
    expect(readSidebarWidth('right')).toBe(300);
    expect(writeSidebarWidth('left', 420)).toBe(420);
    expect(readSidebarWidth('left')).toBe(420);
    expect(readSidebarWidth('right')).toBe(300);
    expect(writeSidebarWidth('right', 610)).toBe(610);
    expect(readSidebarWidth('left')).toBe(420);
    expect(readSidebarWidth('right')).toBe(610);
    expect(writeSidebarWidth('left', 300)).toBe(300);
    expect(readSidebarWidth('left')).toBe(300);
    expect(readSidebarWidth('right')).toBe(610);
  });

  it.each([
    ['', 300],
    ['  ', 300],
    ['oops', 300],
    ['NaN', 300],
    ['Infinity', 300],
    ['-Infinity', 300],
    ['420px', 300],
    ['-20', 200],
    ['900', 720],
    ['420', 420],
  ])('restores stored %s as %s pixels', (stored, expected) => {
    vi.stubGlobal('localStorage', {
      getItem: () => stored,
      setItem: () => {},
    });
    expect(readSidebarWidth('left')).toBe(expected);
  });

  it('stores bounded values', () => {
    expect(writeSidebarWidth('left', 100)).toBe(200);
    expect(readSidebarWidth('left')).toBe(200);
    expect(writeSidebarWidth('right', 900)).toBe(720);
    expect(readSidebarWidth('right')).toBe(720);
  });

  it('returns usable widths when storage access throws', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('quota');
      },
    });
    expect(readSidebarWidth('left')).toBe(300);
    expect(writeSidebarWidth('left', 420)).toBe(420);
    expect(writeSidebarWidth('right', 900)).toBe(720);
  });

  it('tolerates a throwing localStorage getter', () => {
    const descriptor = Object.getOwnPropertyDescriptor(
      globalThis,
      'localStorage'
    );
    Object.defineProperty(globalThis, 'localStorage', {
      configurable: true,
      get: () => {
        throw new Error('denied');
      },
    });
    try {
      expect(readSidebarWidth('left')).toBe(300);
      expect(writeSidebarWidth('right', 480)).toBe(480);
    } finally {
      if (descriptor)
        Object.defineProperty(globalThis, 'localStorage', descriptor);
    }
  });
});
