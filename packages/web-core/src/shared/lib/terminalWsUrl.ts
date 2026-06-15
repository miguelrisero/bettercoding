/**
 * Build the terminal WebSocket URL.
 *
 * `cols`/`rows` are baked into the URL because the backend opens the PTY (and,
 * in CLI mode, the tmux/claude session under it) at exactly that size. Passing
 * the terminal's REAL fitted size — rather than the 80x24 default — means a
 * fresh attach renders at the right dimensions immediately instead of starting
 * small and reflowing on the follow-up `onopen` resize. That reflow of a
 * main-screen TUI (claude) is what stacked junk/blank lines into the pane on
 * every reconnect.
 *
 * Kept as a pure function (location pieces passed in, not read from `window`)
 * so the size-carrying contract is unit-testable.
 */
export interface TerminalWsUrlParams {
  workspaceId: string;
  cols: number;
  rows: number;
  /** `window.location.protocol` (e.g. "https:"). */
  protocol: string;
  /** `window.location.host`. */
  host: string;
  mode?: 'shell' | 'cli';
  /** CLI-mode handover: the uix session whose claude conversation to resume. */
  sessionId?: string;
}

export function buildTerminalWsUrl({
  workspaceId,
  cols,
  rows,
  protocol,
  host,
  mode = 'shell',
  sessionId,
}: TerminalWsUrlParams): string {
  const scheme = protocol === 'https:' ? 'https:' : 'http:';
  const modeParam = mode === 'cli' ? '&mode=cli' : '';
  const sessionParam =
    mode === 'cli' && sessionId ? `&session_id=${sessionId}` : '';
  return `${scheme}//${host}/api/terminal/ws?workspace_id=${workspaceId}&cols=${cols}&rows=${rows}${modeParam}${sessionParam}`;
}
