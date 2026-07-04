import { useCallback, useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import '@xterm/xterm/css/xterm.css';

import {
  TERMINAL_BACKGROUND,
  getTerminalTheme,
} from '@/shared/lib/terminalTheme';
import { buildTerminalWsUrl } from '@/shared/lib/terminalWsUrl';
import { cancelActiveTerminalGesture } from '@/shared/lib/terminalTouchGestures';
import { installTerminalTouchLayers } from '@/shared/lib/terminalTouchLayers';
import { applyStickyCtrl } from '@/shared/lib/terminalKeySequences';
import {
  isTerminalPasting,
  pasteTextIntoTerminal,
} from '@/shared/lib/terminalPaste';
import {
  getTerminalMobileState,
  patchTerminalMobileState,
} from '@/shared/lib/terminalMobileState';
import {
  loadMobileTerminalFontSize,
  TERMINAL_DEFAULT_FONT_SIZE,
} from '@/shared/lib/terminalFontSize';
import { isTouchDevice } from '@/shared/hooks/useIsMobile';
import { useTerminal } from '@/shared/hooks/useTerminal';
import { TerminalMobileControls } from './TerminalMobileControls';
import { TerminalKeyBar } from './TerminalKeyBar';

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
  // State mirror of terminalRef for children that must re-render/subscribe
  // when the live terminal (re)attaches (key bar latch highlight, controls).
  const [liveTerminal, setLiveTerminal] = useState<Terminal | null>(null);
  const {
    registerTerminalInstance,
    getTerminalInstance,
    createTerminalConnection,
    getTerminalConnection,
  } = useTerminal();

  // Built with the terminal's CURRENT grid size (see buildTerminalWsUrl): a
  // fresh attach must open the PTY at the real size, not the 80x24 default, or
  // claude reflows on the follow-up onopen resize and stacks blank lines every
  // time the CLI pane is reopened.
  const buildEndpoint = useCallback(
    (cols: number, rows: number) =>
      buildTerminalWsUrl({
        workspaceId,
        cols,
        rows,
        protocol: window.location.protocol,
        host: window.location.host,
        mode,
        sessionId,
      }),
    [workspaceId, mode, sessionId]
  );

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
        // Touch devices restore the persisted A−/A+ stepper choice; desktop
        // keeps the fixed default.
        fontSize: isTouchDevice()
          ? loadMobileTerminalFontSize()
          : TERMINAL_DEFAULT_FONT_SIZE,
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

      registerTerminalInstance(tabId, terminal, fitAddon);

      // Sticky Ctrl (mobile key bar): a latched ctrl turns the next single
      // TYPED character into its control code. Provenance rules: pastes are
      // bracketed by the isTerminalPasting marker (all app paste paths go
      // through pasteTextIntoTerminal; xterm's paste() emits synchronously) —
      // clipboard text is never transformed. Terminal query replies (DSR/DA)
      // are always multi-char and pass through untouched. Everything else
      // that arrives as a single character is a keystroke — including Android
      // IME commits, which xterm delivers via composition WITHOUT a key
      // event, so an onKey-based flag would leave the latch dead on Gboard.
      // The latch stays armed until a keystroke consumes it.
      terminal.onData((data) => {
        const conn = getTerminalConnection(tabId);
        if (!conn) return;
        if (
          data.length === 1 &&
          !isTerminalPasting(terminal) &&
          getTerminalMobileState(terminal).ctrlLatched
        ) {
          patchTerminalMobileState(terminal, { ctrlLatched: false });
          conn.send(applyStickyCtrl(data).out);
          return;
        }
        conn.send(data);
      });

      // Windows-console clipboard ergonomics. Selecting text copies it
      // immediately (no keystroke needed); right-click copies the selection
      // when one exists, otherwise pastes. Both attach once per created
      // terminal and live exactly as long as it does (the listener dies with
      // the element on dispose, the selection hook with the terminal).
      terminal.onSelectionChange(() => {
        // Touch select mode fires a selection change per touchmove — copying
        // here would hammer the clipboard with intermediate selections (and
        // destroy whatever the user had copied the moment a drag starts).
        // That mode copies exactly once on release (terminalTouchSelection).
        if (getTerminalMobileState(terminal).selectMode) return;
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
              // Marker-carrying paste — never sticky-Ctrl-transformed.
              if (text) pasteTextIntoTerminal(terminal, text);
            })
            .catch(() => {
              // Paste permission denied — Ctrl+V still works via the
              // terminal's own textarea.
            });
        }
      });

      // All touch layers (gestures → selection → scroll bridge) in the one
      // valid order — see terminalTouchLayers for the ordering invariant and
      // lifetime rules. Attached once per created terminal; NOT removed in
      // the mount cleanup below — the listeners live and die with the
      // element on terminal.dispose(), like the handlers above.
      installTerminalTouchLayers(terminal, (data) =>
        getTerminalConnection(tabId)?.send(data)
      );
    }

    terminalRef.current = terminal;
    fitAddonRef.current = fitAddon;
    setLiveTerminal(terminal);

    // Ensure a backend connection exists for this tab — also on the reattach
    // path, so a tab whose connection died (e.g. reconnect gave up, server
    // restarted) heals on the next mount instead of staying dead.
    if (!getTerminalConnection(tabId)) {
      // Connect at the fitted size (set by fit() above) so the backend opens
      // the PTY at the real dimensions — no 80x24-then-resize reflow.
      createTerminalConnection(
        tabId,
        buildEndpoint(terminal.cols, terminal.rows),
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
      // A D-pad gesture running right now would never see its touchend once
      // the element leaves the DOM — stop its repeat timer and release the
      // scroll-bridge suppression before detaching. Modes also reset: a
      // select mode or armed ctrl latch silently surviving a pane
      // close/reopen (the terminal persists in the registry) would leave
      // swipes selecting instead of scrolling, or fire a control code
      // minutes later, with no visible cue at remount.
      cancelActiveTerminalGesture(terminal);
      patchTerminalMobileState(terminal, {
        selectMode: false,
        ctrlLatched: false,
      });
      if (terminal.element && terminal.element.parentNode) {
        terminal.element.parentNode.removeChild(terminal.element);
      }
      terminalRef.current = null;
      fitAddonRef.current = null;
      setLiveTerminal(null);
    };
  }, [
    tabId,
    buildEndpoint,
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

  const sendKey = useCallback(
    (data: string) => {
      getTerminalConnection(tabId)?.send(data);
    },
    [tabId, getTerminalConnection]
  );

  return (
    // The padding ring is painted terminal-black (not the app surface color)
    // so the always-dark terminal doesn't sit in a light frame on the light
    // theme.
    <div
      ref={resizeRef}
      className="w-full h-full flex flex-col px-2 py-1 overscroll-contain"
      style={{ background: TERMINAL_BACKGROUND }}
    >
      <div className="relative flex-1 min-h-0">
        <div ref={containerRef} className="w-full h-full" />
        <TerminalMobileControls terminal={liveTerminal} refit={fitTerminal} />
      </div>
      {/* Touch-only hotkey row (renders null off-touch). Sits at the pane's
          bottom edge — the visual-viewport sizing keeps that edge above the
          on-screen keyboard, Termius-style. */}
      <TerminalKeyBar terminal={liveTerminal} onSendKey={sendKey} />
    </div>
  );
}
