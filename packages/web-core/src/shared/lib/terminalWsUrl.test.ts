import { describe, it, expect } from 'vitest';

import { buildTerminalWsUrl } from './terminalWsUrl';

describe('buildTerminalWsUrl', () => {
  it('carries the provided grid size instead of the 80x24 default', () => {
    const url = buildTerminalWsUrl({
      workspaceId: 'ws-1',
      cols: 203,
      rows: 51,
      protocol: 'https:',
      host: 'example.test',
      mode: 'cli',
    });
    expect(url).toContain('cols=203');
    expect(url).toContain('rows=51');
    // The whole point of the fix: never the default that caused the reflow.
    expect(url).not.toContain('cols=80');
    expect(url).not.toContain('rows=24');
  });

  it('adds &mode=cli and &session_id only in CLI mode', () => {
    const cli = buildTerminalWsUrl({
      workspaceId: 'ws-1',
      cols: 80,
      rows: 24,
      protocol: 'http:',
      host: 'h',
      mode: 'cli',
      sessionId: 'sess-9',
    });
    expect(cli).toContain('&mode=cli');
    expect(cli).toContain('&session_id=sess-9');

    const shell = buildTerminalWsUrl({
      workspaceId: 'ws-1',
      cols: 80,
      rows: 24,
      protocol: 'http:',
      host: 'h',
      mode: 'shell',
      sessionId: 'sess-9',
    });
    expect(shell).not.toContain('mode=');
    expect(shell).not.toContain('session_id');
  });

  it('maps a non-https protocol to http and preserves host + workspace', () => {
    const url = buildTerminalWsUrl({
      workspaceId: 'abc',
      cols: 10,
      rows: 10,
      protocol: 'http:',
      host: 'localhost:3000',
    });
    expect(url.startsWith('http://localhost:3000/api/terminal/ws?')).toBe(true);
    expect(url).toContain('workspace_id=abc');
  });
});
