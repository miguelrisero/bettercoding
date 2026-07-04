import { useCallback, useEffect, useRef, useState } from 'react';
import type { Terminal } from '@xterm/xterm';
import {
  CopyIcon,
  ClipboardTextIcon,
  CursorTextIcon,
  KeyboardIcon,
  CaretLeftIcon,
  CaretRightIcon,
} from '@phosphor-icons/react';

import { cn } from '@/shared/lib/utils';
import { useIsTouchDevice } from '@/shared/hooks/useIsMobile';
import { extractViewportText } from '@/shared/lib/terminalViewportText';
import {
  clampTerminalFontSize,
  saveMobileTerminalFontSize,
  TERMINAL_DEFAULT_FONT_SIZE,
} from '@/shared/lib/terminalFontSize';
import {
  getTerminalMobileState,
  patchTerminalMobileState,
  subscribeTerminalMobileFlash,
  useTerminalMobileState,
} from '@/shared/lib/terminalMobileState';
import { pasteIntoTerminal } from '@/shared/lib/terminalPaste';

interface TerminalMobileControlsProps {
  /** Live terminal (null until the mount effect registers it). */
  terminal: Terminal | null;
  /** Re-fit the grid + report the new size to the PTY (font-size steppers). */
  refit: () => void;
}

const STATUS_MS = 1600;

const BUTTON_CLASS =
  'flex items-center justify-center size-11 rounded-md bg-secondary border ' +
  'text-low hover:text-normal active:bg-primary transition-colors';

/**
 * Touch-only Copy / Paste / Select / font-size / Keyboard affordances for the
 * terminal. Desktop keeps its mouse/keyboard flow (drag-select, right-click,
 * Ctrl/Cmd+V) untouched — this renders nothing unless the device is
 * touch-capable.
 *
 * Mounted as a sibling of (NOT inside) the xterm element so taps never reach
 * xterm's focus/selection handling. Collapsible and pinned top-right so it
 * can't cover claude's bottom input. Every action gives explicit feedback
 * (mobile clipboard calls fail silently otherwise).
 */
export function TerminalMobileControls({
  terminal,
  refit,
}: TerminalMobileControlsProps) {
  const isTouch = useIsTouchDevice();
  const [expanded, setExpanded] = useState(true);
  const [status, setStatus] = useState<string | null>(null);
  const statusTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const { selectMode } = useTerminalMobileState(terminal);

  useEffect(
    () => () => {
      if (statusTimer.current) clearTimeout(statusTimer.current);
    },
    []
  );

  const flash = useCallback((msg: string) => {
    setStatus(msg);
    if (statusTimer.current) clearTimeout(statusTimer.current);
    statusTimer.current = setTimeout(() => setStatus(null), STATUS_MS);
  }, []);

  // Surface one-shot status messages emitted by the DOM-level installers
  // (e.g. the three-finger paste gesture) in the same flash pill.
  useEffect(() => {
    if (!terminal) return;
    return subscribeTerminalMobileFlash(terminal, flash);
  }, [terminal, flash]);

  if (!isTouch) return null;

  const handleKeyboard = () => {
    terminal?.focus();
  };

  const handlePaste = () => {
    if (!terminal) return;
    void pasteIntoTerminal(terminal, flash);
  };

  const handleCopy = async () => {
    if (!terminal) return;
    // Guard the write API up front so we never flash "Copied" without copying.
    if (!navigator.clipboard?.writeText) {
      flash('Copy unavailable');
      return;
    }
    let text: string;
    let label: string;
    if (terminal.hasSelection()) {
      text = terminal.getSelection();
      label = 'Copied selection';
    } else {
      const buf = terminal.buffer.active;
      text = extractViewportText(buf, buf.viewportY, terminal.rows);
      label = 'Copied screen';
    }
    if (!text) {
      flash('Nothing to copy');
      return;
    }
    try {
      await navigator.clipboard.writeText(text);
      flash(label);
    } catch {
      flash('Copy blocked');
    }
  };

  const handleSelectMode = () => {
    if (!terminal) return;
    const next = !getTerminalMobileState(terminal).selectMode;
    patchTerminalMobileState(terminal, { selectMode: next });
    if (!next) terminal.clearSelection();
    flash(next ? 'Select mode — drag to select' : 'Select mode off');
  };

  const stepFontSize = (delta: number) => {
    if (!terminal) return;
    const current = terminal.options.fontSize ?? TERMINAL_DEFAULT_FONT_SIZE;
    const next = clampTerminalFontSize(current + delta);
    if (next === current) {
      flash(`Font ${current}px (limit)`);
      return;
    }
    terminal.options.fontSize = next;
    saveMobileTerminalFontSize(next);
    refit();
    flash(`Font ${next}px`);
  };

  const actions = [
    { label: 'Copy from terminal', Icon: CopyIcon, onClick: handleCopy },
    {
      label: 'Paste into terminal',
      Icon: ClipboardTextIcon,
      onClick: handlePaste,
    },
    { label: 'Show keyboard', Icon: KeyboardIcon, onClick: handleKeyboard },
  ];

  // Belt-and-suspenders: keep taps on the controls from reaching the terminal.
  const stop = (e: { stopPropagation: () => void }) => e.stopPropagation();

  return (
    <div
      className="absolute top-1 right-1 z-10 flex items-center gap-1"
      style={{
        paddingTop: 'env(safe-area-inset-top, 0px)',
        paddingRight: 'env(safe-area-inset-right, 0px)',
      }}
      onPointerDown={stop}
      onTouchStart={stop}
      onMouseDown={stop}
      onContextMenu={stop}
    >
      {status && (
        <span
          role="status"
          aria-live="polite"
          className="rounded bg-secondary border px-2 py-1 text-xs text-normal"
        >
          {status}
        </span>
      )}
      {expanded && (
        <>
          {actions.map(({ label, Icon, onClick }) => (
            <button
              key={label}
              type="button"
              className={BUTTON_CLASS}
              aria-label={label}
              onClick={onClick}
            >
              <Icon className="size-icon-sm" weight="bold" aria-hidden="true" />
            </button>
          ))}
          <button
            type="button"
            className={cn(BUTTON_CLASS, selectMode && 'bg-primary text-normal')}
            aria-label="Toggle select mode"
            aria-pressed={selectMode}
            onClick={handleSelectMode}
          >
            <CursorTextIcon
              className="size-icon-sm"
              weight="bold"
              aria-hidden="true"
            />
          </button>
          <button
            type="button"
            className={cn(BUTTON_CLASS, 'text-xs font-medium')}
            aria-label="Decrease terminal font size"
            onClick={() => stepFontSize(-1)}
          >
            A−
          </button>
          <button
            type="button"
            className={cn(BUTTON_CLASS, 'text-xs font-medium')}
            aria-label="Increase terminal font size"
            onClick={() => stepFontSize(1)}
          >
            A+
          </button>
        </>
      )}
      <button
        type="button"
        className={cn(BUTTON_CLASS, 'opacity-80')}
        aria-label={
          expanded ? 'Hide terminal controls' : 'Show terminal controls'
        }
        aria-expanded={expanded}
        onClick={() => setExpanded((v) => !v)}
      >
        {expanded ? (
          <CaretRightIcon
            className="size-icon-sm"
            weight="bold"
            aria-hidden="true"
          />
        ) : (
          <CaretLeftIcon
            className="size-icon-sm"
            weight="bold"
            aria-hidden="true"
          />
        )}
      </button>
    </div>
  );
}
