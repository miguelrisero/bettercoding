import { describe, expect, it } from 'vitest';
import { fromUtcControl, toUtcControl } from './inputSchedule';

describe('UTC Input schedule', () => {
  it('converts the control without the browser timezone', () => {
    expect(fromUtcControl('2026-11-02T08:00')).toBe('2026-11-02T08:00:00.000Z');
    expect(toUtcControl('2026-11-02T09:00:00+01:00')).toBe('2026-11-02T08:00');
    expect(toUtcControl(null)).toBe('');
  });
  it.each([
    '',
    '2026-02-30T08:00',
    '2026-02-29T08:00',
    '2026-13-01T08:00',
    '2026-11-02T24:00',
    '2026-11-02T08:60',
    '2026-11-02T08:00Z',
    '0000-01-01T00:00',
    '10000-01-01T00:00',
  ])('rejects invalid UTC calendar value %s', (value) => {
    expect(() => fromUtcControl(value)).toThrow(
      'Enter a valid UTC date and time.'
    );
  });
  it('accepts a leap day and past timestamps', () => {
    expect(fromUtcControl('2024-02-29T08:00')).toBe('2024-02-29T08:00:00.000Z');
  });
});
