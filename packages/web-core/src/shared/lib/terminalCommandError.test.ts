import { describe, expect, it } from 'vitest';

import { handleTerminalCommandError } from './terminalCommandError';

describe('terminal command rejection', () => {
  it('keeps the connection for explicit resubmission after custody clears', () => {
    const delivered: string[] = [];
    const displayed: string[] = [];
    let busy = true;
    const connections = new Map([
      [
        'terminal',
        (text: string) => {
          if (!busy) delivered.push(text);
        },
      ],
    ]);
    const resubmit = () => connections.get('terminal')?.('owner input');
    resubmit();
    handleTerminalCommandError(
      { type: 'input_rejected', message: 'CLI control busy; resubmit' },
      () => connections.delete('terminal'),
      (text) => displayed.push(text)
    );
    expect(connections.has('terminal')).toBe(true);
    expect(delivered).toEqual([]);
    expect(displayed[0]).toContain('CLI control busy; resubmit');
    busy = false;
    resubmit();
    expect(delivered).toEqual(['owner input']);
  });

  it('releases the connection for fatal backend errors', () => {
    let released = false;
    handleTerminalCommandError(
      { type: 'error', message: 'Failed to create PTY' },
      () => {
        released = true;
      },
      () => {}
    );
    expect(released).toBe(true);
  });
});
