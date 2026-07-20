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
  it('orders a hidden-to-visible resize before the exact presence frame', () => {
    const log: string[] = [];
    const connection: TerminalPresenceConnection = {
      ws: {
        send: (data) => log.push(String(data)),
      },
      resize: (cols, rows) => log.push(`resize:${cols}x${rows}`),
      lastSentPresence: false,
    };

    sendPresence(connection, SIZE, 'visible', {
      resendVisibleSize: true,
    });

    expect(log).toEqual([
      'resize:120x40',
      '{"type":"presence","visible":true}',
    ]);
  });

  it('publishes only effective-presence transitions unless forced', () => {
    const frames: string[] = [];
    const connection: TerminalPresenceConnection = {
      ws: {
        send: (data) => frames.push(String(data)),
      },
      resize: () => {},
      lastSentPresence: null,
    };

    sendPresence(connection, SIZE, 'visible');
    sendPresence(connection, SIZE, 'visible');
    sendPresence(connection, null, 'visible');
    sendPresence(connection, SIZE, 'hidden');
    sendPresence(connection, SIZE, 'visible');

    expect(frames.map((frame) => JSON.parse(frame).visible)).toEqual([
      true,
      false,
      true,
    ]);
    expect(connection.lastSentPresence).toBe(true);

    sendPresence(connection, SIZE, 'visible', { force: true });
    expect(frames).toHaveLength(4);
  });
});
