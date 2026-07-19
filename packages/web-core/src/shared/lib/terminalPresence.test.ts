import { describe, expect, it } from 'vitest';

import {
  getEffectiveTerminalPresence,
  sendPresence,
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
