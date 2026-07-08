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

/**
 * Resolve the WS endpoint from a freshly measured grid, or `null` when the pane
 * could not be measured.
 *
 * `dims` is `FitAddon.proposeDimensions()`, which returns `undefined` for a
 * hidden (`display:none`) or 0-height container. Returning `null` in that case
 * is the load-bearing invariant of the stray-newline fix: the caller must NEVER
 * open the WS at a placeholder size (xterm's 80x24 constructor default), because
 * the backend opens the PTY/tmux at exactly the URL size and claude then reflows
 * — stacking blank lines that read as a stray Enter — on the follow-up resize
 * once the pane becomes visible. A `null` here means "defer / reschedule the
 * connect until the pane is measurable".
 *
 * Because it re-reads the CURRENT dims on every call, using it as the per-attempt
 * endpoint source also makes reconnects attach at the pane's present grid rather
 * than the size frozen when the tab was first created.
 */
export function resolveTerminalEndpoint(
  params: Omit<TerminalWsUrlParams, 'cols' | 'rows'>,
  dims: { cols: number; rows: number } | undefined | null
): string | null {
  // Number.isFinite also rejects NaN: FitAddon.proposeDimensions() derives
  // its numbers from parseInt(getComputedStyle(...)) and can yield NaN for a
  // detached or oddly-styled container — that must defer like any other
  // unmeasured pane, not bake "cols=NaN" into the URL (the server would
  // reject it and the reconnect ladder would burn its budget to giveUp).
  if (
    !dims ||
    !Number.isFinite(dims.cols) ||
    !Number.isFinite(dims.rows) ||
    dims.cols <= 0 ||
    dims.rows <= 0
  ) {
    return null;
  }
  return buildTerminalWsUrl({ ...params, cols: dims.cols, rows: dims.rows });
}

/**
 * Compare two terminal WS endpoints ignoring the size params (`cols`/`rows`).
 *
 * Used to validate an in-flight connect attempt after its async open
 * resolves: size drift between "attempt started" and "attempt resolved" is
 * legitimate (the post-open resize corrects it), but a changed `session_id`,
 * `mode`, or `workspace_id` — e.g. a session switch while a slow relay
 * transport was opening — makes the attempt STALE: registering it would bind
 * the visible pane to the previous conversation's tmux session.
 */
export function terminalEndpointsEquivalent(a: string, b: string): boolean {
  try {
    const ua = new URL(a);
    const ub = new URL(b);
    if (ua.origin !== ub.origin || ua.pathname !== ub.pathname) return false;
    for (const key of ['cols', 'rows']) {
      ua.searchParams.delete(key);
      ub.searchParams.delete(key);
    }
    ua.searchParams.sort();
    ub.searchParams.sort();
    return ua.searchParams.toString() === ub.searchParams.toString();
  } catch {
    // Not parseable as URLs (unexpected) — fall back to exact equality.
    return a === b;
  }
}
