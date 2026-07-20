export interface TerminalSize {
  cols: number;
  rows: number;
}

export interface TerminalPresenceConnection {
  ws: Pick<WebSocket, 'send'>;
  resize: (cols: number, rows: number) => void;
  lastSentPresence: boolean | null;
}

export interface SendPresenceOptions {
  force?: boolean;
  resendVisibleSize?: boolean;
}

/**
 * Keep the pane-fit path's resize and effective-presence publication coupled.
 * A null size means the pane is unmeasurable: publish its demotion without
 * ever forwarding FitAddon's clamped placeholder grid.
 */
export function syncTerminalFitState(
  connection: Pick<TerminalPresenceConnection, 'resize'>,
  size: TerminalSize | null,
  publishPresence: () => void
): void {
  if (size) {
    connection.resize(size.cols, size.rows);
  }
  publishPresence();
}

export function getEffectiveTerminalPresence(
  visibilityState: DocumentVisibilityState,
  size: TerminalSize | null
): boolean {
  return visibilityState === 'visible' && size !== null;
}

export function sendPresence(
  connection: TerminalPresenceConnection,
  size: TerminalSize | null,
  visibilityState: DocumentVisibilityState,
  { force = false, resendVisibleSize = false }: SendPresenceOptions = {}
): void {
  const visible = getEffectiveTerminalPresence(visibilityState, size);
  if (!force && connection.lastSentPresence === visible) {
    return;
  }

  // Re-admitting a previously hidden client at its stale grid would reflow
  // tmux once here and again on resize. Refresh its grid first, then clear
  // ignore-size with the presence frame.
  if (visible && resendVisibleSize && size) {
    connection.resize(size.cols, size.rows);
  }
  connection.ws.send(JSON.stringify({ type: 'presence', visible }));
  connection.lastSentPresence = visible;
}
