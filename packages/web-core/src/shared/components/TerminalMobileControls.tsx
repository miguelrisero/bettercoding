import { useEffect, useRef, useState } from 'react';
import type { Terminal } from '@xterm/xterm';
import {
  CopyIcon,
  ClipboardTextIcon,
  KeyboardIcon,
  CaretLeftIcon,
  CaretRightIcon,
} from '@phosphor-icons/react';

import { cn } from '@/shared/lib/utils';
import { useIsTouchDevice } from '@/shared/hooks/useIsTouchDevice';
import { extractViewportText } from '@/shared/lib/terminalViewportText';

interface TerminalMobileControlsProps {
  /** Live terminal accessor — refs don't trigger renders, so read on demand. */
  getTerminal: () => Terminal | null;
}

const STATUS_MS = 1600;

/**
 * Touch-only Copy / Paste / Keyboard affordances for the terminal. Desktop keeps
 * its mouse/keyboard flow (drag-select, right-click, Ctrl/Cmd+V) untouched — this
 * renders nothing unless the device is touch-capable.
 *
 * Mounted as a sibling of (NOT inside) the xterm element so taps never reach
 * xterm's focus/selection handling. Collapsible and pinned top-right so it can't
 * cover claude's bottom input. Every action gives explicit feedback (mobile
 * clipboard calls fail silently otherwise).
 */
export function TerminalMobileControls({
  getTerminal,
}: TerminalMobileControlsProps) {
  const isTouch = useIsTouchDevice();
  const [expanded, setExpanded] = useState(true);
  const [status, setStatus] = useState<string | null>(null);
  const statusTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (statusTimer.current) clearTimeout(statusTimer.current);
    },
    []
  );

  if (!isTouch) return null;

  const flash = (msg: string) => {
    setStatus(msg);
    if (statusTimer.current) clearTimeout(statusTimer.current);
    statusTimer.current = setTimeout(() => setStatus(null), STATUS_MS);
  };

  const handleKeyboard = () => {
    getTerminal()?.focus();
  };

  const handlePaste = async () => {
    const term = getTerminal();
    if (!term) return;
    try {
      const text = await navigator.clipboard?.readText();
      if (!text) {
        flash('Clipboard empty');
        return;
      }
      term.paste(text);
      flash('Pasted');
    } catch {
      flash('Paste blocked');
    }
  };

  const handleCopy = async () => {
    const term = getTerminal();
    if (!term) return;
    let text: string;
    let label: string;
    if (term.hasSelection()) {
      text = term.getSelection();
      label = 'Copied selection';
    } else {
      const buf = term.buffer.active;
      text = extractViewportText(buf, buf.viewportY, term.rows);
      label = 'Copied screen';
    }
    if (!text) {
      flash('Nothing to copy');
      return;
    }
    try {
      await navigator.clipboard?.writeText(text);
      flash(label);
    } catch {
      flash('Copy blocked');
    }
  };

  // Belt-and-suspenders: keep taps on the controls from reaching the terminal.
  const stop = (e: { stopPropagation: () => void }) => e.stopPropagation();

  const button =
    'flex items-center justify-center size-11 rounded-md bg-secondary border ' +
    'text-low hover:text-normal active:bg-primary transition-colors';

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
          <button
            type="button"
            className={button}
            aria-label="Copy from terminal"
            onClick={handleCopy}
          >
            <CopyIcon
              className="size-icon-sm"
              weight="bold"
              aria-hidden="true"
            />
          </button>
          <button
            type="button"
            className={button}
            aria-label="Paste into terminal"
            onClick={handlePaste}
          >
            <ClipboardTextIcon
              className="size-icon-sm"
              weight="bold"
              aria-hidden="true"
            />
          </button>
          <button
            type="button"
            className={button}
            aria-label="Show keyboard"
            onClick={handleKeyboard}
          >
            <KeyboardIcon
              className="size-icon-sm"
              weight="bold"
              aria-hidden="true"
            />
          </button>
        </>
      )}
      <button
        type="button"
        className={cn(button, 'opacity-80')}
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
