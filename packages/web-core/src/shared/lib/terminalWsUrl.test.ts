import { describe, it, expect } from 'vitest';

import { buildTerminalWsUrl, resolveTerminalEndpoint } from './terminalWsUrl';

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

describe('resolveTerminalEndpoint', () => {
  const base = {
    workspaceId: 'ws-1',
    protocol: 'https:',
    host: 'example.test',
    mode: 'cli' as const,
  };

  it('returns null when the pane is unmeasured (hidden / 0-height)', () => {
    // FitAddon.proposeDimensions() is undefined for a display:none container.
    expect(resolveTerminalEndpoint(base, undefined)).toBeNull();
    expect(resolveTerminalEndpoint(base, null)).toBeNull();
    // A 0-sized container must never bake xterm's 80x24 default into the URL.
    expect(resolveTerminalEndpoint(base, { cols: 0, rows: 0 })).toBeNull();
    expect(resolveTerminalEndpoint(base, { cols: 120, rows: 0 })).toBeNull();
  });

  it('builds a URL carrying the real measured grid when measurable', () => {
    const url = resolveTerminalEndpoint(base, { cols: 203, rows: 51 });
    expect(url).not.toBeNull();
    expect(url).toContain('cols=203');
    expect(url).toContain('rows=51');
    expect(url).toContain('&mode=cli');
    // Never the default that caused the reflow.
    expect(url).not.toContain('cols=80');
    expect(url).not.toContain('rows=24');
  });

  it('reflects the CURRENT grid on each call, so a reconnect re-fits', () => {
    // Models the per-attempt getEndpoint: connectWebSocket calls it fresh on
    // every (re)connect, so a size change between attempts yields a new URL
    // instead of replaying the size frozen at tab creation.
    let dims = { cols: 100, rows: 30 };
    const getEndpoint = () => resolveTerminalEndpoint(base, dims);

    const first = getEndpoint();
    dims = { cols: 120, rows: 40 };
    const second = getEndpoint();

    expect(first).toContain('cols=100');
    expect(second).toContain('cols=120');
    expect(second).not.toBe(first);
  });
});
