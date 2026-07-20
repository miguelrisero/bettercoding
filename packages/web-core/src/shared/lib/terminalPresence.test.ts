import { describe, expect, it } from 'vitest';

import {
  getEffectiveTerminalPresence,
  sendPresence,
  syncTerminalFitState,
  type TerminalPresenceConnection,
} from './terminalPresence';

const SIZE = { cols: 120, rows: 40 };

describe('getEffectiveTerminalPresence', () => {
  it('requires both a visible document and a measurable pane', () => {
    expect(getEffectiveTerminalPresence('hidden', SIZE)).toBe(false);
    expect(getEffectiveTerminalPresence('visible', null)).toBe(false);
    expect(getEffectiveTerminalPresence('visible', SIZE)).toBe(true);
  });
});

describe('syncTerminalFitState', () => {
  it('pins pane-fit wiring to resize before republishing presence', () => {
    const log: string[] = [];
    syncTerminalFitState(
      {
        resize: (cols, rows) => log.push(`resize:${cols}x${rows}`),
      },
      SIZE,
      () => log.push('presence')
    );

    expect(log).toEqual(['resize:120x40', 'presence']);
  });

  it('pins hidden-pane wiring to publish presence without resizing', () => {
    const log: string[] = [];
    syncTerminalFitState(
      {
        resize: (cols, rows) => log.push(`resize:${cols}x${rows}`),
      },
      null,
      () => log.push('presence')
    );

    expect(log).toEqual(['presence']);
  });
});

describe('sendPresence', () => {
  it('publishes only effective-presence transitions unless forced', () => {
    const frames: string[] = [];
    const resizes: Array<[number, number]> = [];
    const connection: TerminalPresenceConnection = {
      ws: {
        send: (data) => frames.push(String(data)),
      },
      resize: (cols, rows) => resizes.push([cols, rows]),
      lastSentPresence: null,
    };

    sendPresence(connection, SIZE, 'visible', {
      resendVisibleSize: true,
    });
    sendPresence(connection, SIZE, 'visible', {
      resendVisibleSize: true,
    });
    sendPresence(connection, null, 'visible');
    sendPresence(connection, SIZE, 'hidden');
    sendPresence(connection, SIZE, 'visible');

    expect(frames.map((frame) => JSON.parse(frame).visible)).toEqual([
      true,
      false,
      true,
    ]);
    expect(resizes).toEqual([[120, 40]]);
    expect(connection.lastSentPresence).toBe(true);

    sendPresence(connection, SIZE, 'visible', { force: true });
    expect(frames).toHaveLength(4);
  });
});
