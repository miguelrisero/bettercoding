import { useCallback, useEffect, useMemo, useRef } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import '@xterm/xterm/css/xterm.css';

import { useTheme } from '@/shared/hooks/useTheme';
import { getTerminalTheme } from '@/shared/lib/terminalTheme';
import { useTerminal } from '@/shared/hooks/useTerminal';

interface XTermInstanceProps {
  tabId: string;
  workspaceId: string;
  isActive: boolean;
  onClose?: () => void;
  /**
   * Terminal backend mode. 'shell' (default) is a plain interactive shell;
   * 'cli' attaches the workspace's persistent tmux-backed `claude` session.
   */
  mode?: 'shell' | 'cli';
  /**
   * In 'cli' mode, the VibeKanban session whose claude conversation to resume
   * (handover from the chat UI). Forwarded to the terminal WS as `session_id`.
   */
  sessionId?: string;
}

export function XTermInstance({
  tabId,
  workspaceId,
  isActive,
  onClose,
  mode = 'shell',
  sessionId,
}: XTermInstanceProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const resizeRef = useRef<HTMLDivElement>(null);
  const terminalRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const initialSizeRef = useRef({ cols: 80, rows: 24 });
  const { theme } = useTheme();
  const {
    registerTerminalInstance,
    getTerminalInstance,
    createTerminalConnection,
    getTerminalConnection,
  } = useTerminal();

  const endpoint = useMemo(() => {
    const protocol = window.location.protocol === 'https:' ? 'https:' : 'http:';
    const host = window.location.host;
    const modeParam = mode === 'cli' ? '&mode=cli' : '';
    const sessionParam =
      mode === 'cli' && sessionId ? `&session_id=${sessionId}` : '';
    return `${protocol}//${host}/api/terminal/ws?workspace_id=${workspaceId}&cols=${initialSizeRef.current.cols}&rows=${initialSizeRef.current.rows}${modeParam}${sessionParam}`;
  }, [workspaceId, mode, sessionId]);

  const fitTerminal = useCallback(() => {
    fitAddonRef.current?.fit();
    if (terminalRef.current) {
      const conn = getTerminalConnection(tabId);
      conn?.resize(terminalRef.current.cols, terminalRef.current.rows);
    }
  }, [tabId, getTerminalConnection]);

  // Terminal + connection lifecycle. Every run of this effect MUST register
  // the same cleanup: an early return without one leaves `terminalRef`
  // pointing at the previous tab's (possibly disposed) terminal across a
  // `tabId` change, which used to permanently disarm terminal creation for
  // every workspace visited afterwards (the "CLI panes all blank after
  // switching" bug — the old code had `if (terminalRef.current) return;`
  // after a cleanup-less reattach run).
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    let terminal: Terminal;
    let fitAddon: FitAddon;

    const existing = getTerminalInstance(tabId);
    if (existing) {
      // Reattach a live terminal preserved across remounts (side terminals
      // keep their instance registered while hidden).
      terminal = existing.terminal;
      fitAddon = existing.fitAddon;
      if (terminal.element) {
        container.appendChild(terminal.element);
        fitAddon.fit();
      }
    } else {
      terminal = new Terminal({
        cursorBlink: true,
        fontSize: 12,
        fontFamily: '"IBM Plex Mono", monospace',
        theme: getTerminalTheme(),
      });

      fitAddon = new FitAddon();
      const webLinksAddon = new WebLinksAddon();

      terminal.loadAddon(fitAddon);
      terminal.loadAddon(webLinksAddon);
      terminal.open(container);

      // OSC 52 → system clipboard. This is how tmux-side selections (CLI
      // mode: drag in the pane, tmux copies on release with
      // `set-clipboard on`) land in the browser clipboard — select-to-copy
      // without xterm.js ever owning the selection. Hand-rolled instead of
      // @xterm/addon-clipboard because tmux emits an EMPTY selection field
      // (`ESC]52;;<base64>`), which the addon silently ignores (it only
      // matches an explicit 'c').
      terminal.parser.registerOscHandler(52, (data) => {
        const sep = data.indexOf(';');
        if (sep === -1) return true;
        const selection = data.slice(0, sep);
        const payload = data.slice(sep + 1);
        if (selection !== '' && selection !== 'c' && selection !== 's') {
          return true;
        }
        // '?' is a clipboard READ request — never answer those (a malicious
        // pane process could exfiltrate the clipboard).
        if (payload === '?') return true;
        try {
          const bytes = Uint8Array.from(atob(payload), (ch) =>
            ch.charCodeAt(0)
          );
          const text = new TextDecoder().decode(bytes);
          if (text) {
            void navigator.clipboard?.writeText(text).catch(() => {});
          }
        } catch {
          // Malformed base64 — ignore.
        }
        return true;
      });

      fitAddon.fit();
      initialSizeRef.current = { cols: terminal.cols, rows: terminal.rows };

      registerTerminalInstance(tabId, terminal, fitAddon);

      terminal.onData((data) => {
        const conn = getTerminalConnection(tabId);
        conn?.send(data);
      });

      // Windows-console clipboard ergonomics. Selecting text copies it
      // immediately (no keystroke needed); right-click copies the selection
      // when one exists, otherwise pastes. Both attach once per created
      // terminal and live exactly as long as it does (the listener dies with
      // the element on dispose, the selection hook with the terminal).
      terminal.onSelectionChange(() => {
        const text = terminal.getSelection();
        if (text) {
          void navigator.clipboard?.writeText(text).catch(() => {
            // Clipboard access can be denied (permissions/insecure context);
            // selection itself still works, so fail silently.
          });
        }
      });

      terminal.element?.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        if (terminal.hasSelection()) {
          const text = terminal.getSelection();
          if (text) {
            void navigator.clipboard?.writeText(text).catch(() => {});
          }
          terminal.clearSelection();
        } else {
          void navigator.clipboard
            ?.readText()
            .then((text) => {
              if (text) terminal.paste(text);
            })
            .catch(() => {
              // Paste permission denied — Ctrl+V still works via the
              // terminal's own textarea.
            });
        }
      });
    }

    terminalRef.current = terminal;
    fitAddonRef.current = fitAddon;

    // Ensure a backend connection exists for this tab — also on the reattach
    // path, so a tab whose connection died (e.g. reconnect gave up, server
    // restarted) heals on the next mount instead of staying dead.
    if (!getTerminalConnection(tabId)) {
      createTerminalConnection(
        tabId,
        endpoint,
        (data) => terminal.write(data),
        onClose,
        () => {
          // Re-fit and report the current grid so the PTY/tmux is sized to the
          // pane on every (re)connect (see TerminalProvider ws.onopen).
          fitAddonRef.current?.fit();
          const t = terminalRef.current;
          return t ? { cols: t.cols, rows: t.rows } : null;
        }
      );
    }

    return () => {
      if (terminal.element && terminal.element.parentNode) {
        terminal.element.parentNode.removeChild(terminal.element);
      }
      terminalRef.current = null;
      fitAddonRef.current = null;
    };
  }, [
    tabId,
    endpoint,
    onClose,
    getTerminalInstance,
    registerTerminalInstance,
    createTerminalConnection,
    getTerminalConnection,
  ]);

  useEffect(() => {
    if (!resizeRef.current) return;
    // Debounce: a pane-divider drag fires dozens of observations per second,
    // and each un-coalesced fit() becomes a WS resize frame + SIGWINCH +
    // full TUI redraw. Trailing-edge 75ms keeps the final size exact.
    let timer: ReturnType<typeof setTimeout> | null = null;
    const observer = new ResizeObserver(() => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(fitTerminal, 75);
    });
    observer.observe(resizeRef.current);
    return () => {
      if (timer) clearTimeout(timer);
      observer.disconnect();
    };
  }, [fitTerminal]);

  useEffect(() => {
    if (isActive) terminalRef.current?.focus();
  }, [isActive]);

  useEffect(() => {
    if (terminalRef.current) {
      terminalRef.current.options.theme = getTerminalTheme();
    }
  }, [theme]);

  return (
    <div ref={resizeRef} className="w-full h-full px-2 py-1">
      <div ref={containerRef} className="w-full h-full" />
    </div>
  );
}
