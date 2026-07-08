import { useCallback, useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import '@xterm/xterm/css/xterm.css';

import {
  TERMINAL_BACKGROUND,
  getTerminalTheme,
} from '@/shared/lib/terminalTheme';
import { resolveTerminalEndpoint } from '@/shared/lib/terminalWsUrl';
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

  // Latest sessionId / onClose for the connection callbacks. The provider
  // stores those callbacks for the connection's whole lifetime, which spans
  // session switches and component remounts (a live connection is never torn
  // down on either). Reading the props through refs keeps every later
  // reconnect on the session the UI currently shows — a sessionId captured at
  // connect time could resume the wrong conversation if the tmux session was
  // meanwhile reaped and the reconnect had to recreate it — and keeps the
  // unstable onClose prop identity out of the callback deps (it would
  // otherwise churn the ResizeObserver effect on every parent render).
  const sessionIdRef = useRef(sessionId);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    sessionIdRef.current = sessionId;
    onCloseRef.current = onClose;
  });

  // Registry lookup + re-fit shared by every stored connection callback. The
  // callbacks live as long as the connection, which survives XTermInstance
  // remounts (CliMainPane gate flips, hidden side tabs) — so they resolve the
  // terminal through the provider REGISTRY (same lifetime), never through this
  // component's refs, which are nulled on unmount.
  const fitInstance = useCallback(() => {
    const instance = getTerminalInstance(tabId);
    instance?.fitAddon.fit();
    return instance;
  }, [tabId, getTerminalInstance]);

  // Re-fit and resolve the WS endpoint at the terminal's CURRENT grid. Returns
  // null when the pane is unmeasurable — never connect then: the URL would
  // carry a placeholder/garbage size and claude would reflow (stacking blank
  // lines that read as a stray Enter) on the follow-up resize once the pane
  // is shown. Called fresh on every (re)connect attempt so a reconnect
  // attaches at the pane's present size, not the creation-time one.
  //
  // Measurability is gated on the terminal element's actual DOM box, not just
  // proposeDimensions(): FitAddon clamps its result to >=1 cell even for a
  // hidden-but-rendered (0-height) container, which would otherwise pass the
  // dims check with a garbage 2x1-ish grid and resurrect the tiny-then-resize
  // bounce this fix exists to kill. display:none and detached containers
  // both read as 0x0 here.
  const getEndpoint = useCallback((): string | null => {
    const instance = fitInstance();
    if (!instance) return null;
    const element = instance.terminal.element;
    if (
      !element ||
      !element.isConnected ||
      element.offsetWidth === 0 ||
      element.offsetHeight === 0
    ) {
      return null;
    }
    return resolveTerminalEndpoint(
      {
        workspaceId,
        protocol: window.location.protocol,
        host: window.location.host,
        mode,
        sessionId: sessionIdRef.current,
      },
      instance.fitAddon.proposeDimensions()
    );
  }, [fitInstance, workspaceId, mode]);

  // Open the backend connection for this tab — but only once the pane is
  // actually measurable. A CLI pane can mount inside a hidden (display:none)
  // mobile tab where fit() no-ops; connecting then bakes cols=80&rows=24 into
  // the URL. When unmeasured we DEFER: fitTerminal() re-runs this on the first
  // non-zero ResizeObserver tick (i.e. when the tab is shown). NEVER tears down
  // a live connection — hidden side panes keep theirs by design; this only ever
  // creates the INITIAL connection for a tab that has none.
  const ensureConnection = useCallback(() => {
    // Gate on measurability up front: no connection (and none of the client
    // generation/retry machinery) is set up for a pane that has never been
    // visible. createTerminalConnection itself is idempotent while a live or
    // in-flight connection owns the tab, so calling this repeatedly — mount,
    // ResizeObserver ticks, session switches — never stacks a duplicate
    // backend PTY/tmux attach; while occupied it only refreshes the stored
    // callbacks to this mount's closures.
    if (getEndpoint() === null) return;
    createTerminalConnection(
      tabId,
      getEndpoint,
      // Registry-resolved for the same remount-survival reason as fitInstance:
      // a closure over this component's refs would silently drop all output
      // after a reattach.
      (data) => getTerminalInstance(tabId)?.terminal.write(data),
      () => onCloseRef.current?.(),
      () => {
        // Re-fit and report the current grid so the PTY/tmux is sized to the
        // pane on every (re)connect (see TerminalProvider ws.onopen).
        const instance = fitInstance();
        if (!instance) return null;
        return { cols: instance.terminal.cols, rows: instance.terminal.rows };
      }
    );
  }, [
    tabId,
    getEndpoint,
    fitInstance,
    getTerminalInstance,
    createTerminalConnection,
  ]);

  const fitTerminal = useCallback(() => {
    const instance = fitInstance();
    if (!instance) return;
    const conn = getTerminalConnection(tabId);
    if (conn) {
      conn.resize(instance.terminal.cols, instance.terminal.rows);
    } else {
      // The initial connect was deferred because the pane was unmeasured at
      // mount; now that the ResizeObserver reports a real size, open it.
      ensureConnection();
    }
  }, [tabId, fitInstance, getTerminalConnection, ensureConnection]);

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
    // restarted) heals on the next mount instead of staying dead. Connects at
    // the fitted size, or defers if the pane isn't measurable yet (hidden tab);
    // fitTerminal() opens it on the first non-zero resize once shown.
    ensureConnection();

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
    ensureConnection,
    getTerminalInstance,
    registerTerminalInstance,
    getTerminalConnection,
  ]);

  // A session switch must also heal a dead/given-up connection: without this,
  // a pane whose connection died would only recover on a remount or a resize
  // tick. ensureConnection is idempotent — with a live connection it merely
  // refreshes the stored callbacks (which also re-syncs them after the
  // switch).
  useEffect(() => {
    ensureConnection();
  }, [ensureConnection, sessionId]);

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
