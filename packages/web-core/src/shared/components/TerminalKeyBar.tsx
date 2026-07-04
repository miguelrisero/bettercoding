import { useCallback, useSyncExternalStore } from 'react';
import type { Terminal } from '@xterm/xterm';
import type { Icon } from '@phosphor-icons/react';
import {
  ArrowDownIcon,
  ArrowElbowDownLeftIcon,
  ArrowLeftIcon,
  ArrowLineLeftIcon,
  ArrowLineRightIcon,
  ArrowRightIcon,
  ArrowUpIcon,
} from '@phosphor-icons/react';

import { cn } from '@/shared/lib/utils';
import { useIsTouchDevice } from '@/shared/hooks/useIsMobile';
import { keySequence, type BarKey } from '@/shared/lib/terminalKeySequences';
import {
  getTerminalMobileState,
  patchTerminalMobileState,
  subscribeTerminalMobileState,
} from '@/shared/lib/terminalMobileState';

interface TerminalKeyBarProps {
  /** Live terminal (null until mounted) — used for cursor-mode + ctrl latch state. */
  terminal: Terminal | null;
  /** Send raw bytes to the PTY (bypasses the sticky-Ctrl transform on typed input). */
  onSendKey: (data: string) => void;
}

// Icons instead of key-glyph characters (⇥ ⇧⇥ ⏎): those codepoints are
// missing from Android/Linux system fonts and render as tofu boxes; the
// phosphor icons render identically everywhere.
const KEYS: ReadonlyArray<{
  key: BarKey;
  label?: string;
  Icon?: Icon;
  aria: string;
}> = [
  { key: 'esc', label: 'esc', aria: 'Escape' },
  { key: 'tab', Icon: ArrowLineRightIcon, aria: 'Tab' },
  { key: 'shift-tab', Icon: ArrowLineLeftIcon, aria: 'Shift+Tab' },
  { key: 'ctrl-c', label: '^C', aria: 'Control+C (interrupt)' },
  // 'ctrl' is rendered between ^C and the arrows (special latching button).
  { key: 'left', Icon: ArrowLeftIcon, aria: 'Arrow left' },
  { key: 'down', Icon: ArrowDownIcon, aria: 'Arrow down' },
  { key: 'up', Icon: ArrowUpIcon, aria: 'Arrow up' },
  { key: 'right', Icon: ArrowRightIcon, aria: 'Arrow right' },
  { key: 'enter', Icon: ArrowElbowDownLeftIcon, aria: 'Enter' },
];

const KEY_CLASS =
  'flex items-center justify-center min-w-11 h-9 px-2 rounded-md shrink-0 ' +
  'bg-secondary border text-sm text-low active:bg-primary active:text-normal ' +
  'transition-colors select-none';

/**
 * Termius-style hotkey row for keys mobile soft keyboards can't type:
 * Esc / Tab / Shift+Tab / ^C / sticky Ctrl / arrows / Enter.
 *
 * Touch-only (renders null otherwise) and always visible on touch devices —
 * it sits at the bottom of the terminal pane, which the visual-viewport
 * sizing keeps directly above the on-screen keyboard while typing.
 *
 * Buttons preventDefault on pointerdown so a tap never steals focus from
 * xterm's textarea — the system keyboard stays open while sending keys.
 */
export function TerminalKeyBar({ terminal, onSendKey }: TerminalKeyBarProps) {
  const isTouch = useIsTouchDevice();

  const subscribe = useCallback(
    (cb: () => void) =>
      terminal ? subscribeTerminalMobileState(terminal, cb) : () => {},
    [terminal]
  );
  const ctrlLatched = useSyncExternalStore(
    subscribe,
    () => (terminal ? getTerminalMobileState(terminal).ctrlLatched : false),
    () => false
  );

  if (!isTouch) return null;

  const keepFocus = (e: { preventDefault: () => void }) => e.preventDefault();

  const sendKey = (key: BarKey) => {
    const appCursor = terminal?.modes.applicationCursorKeysMode ?? false;
    onSendKey(keySequence(key, appCursor));
    // A bar tap counts as the latch's "one keystroke" — otherwise ctrl,
    // arrow, then a typed letter would surprise-fire a control combo.
    if (terminal && getTerminalMobileState(terminal).ctrlLatched) {
      patchTerminalMobileState(terminal, { ctrlLatched: false });
    }
    terminal?.scrollToBottom();
  };

  const toggleCtrl = () => {
    if (!terminal) return;
    patchTerminalMobileState(terminal, {
      ctrlLatched: !getTerminalMobileState(terminal).ctrlLatched,
    });
  };

  const renderKey = ({ key, label, Icon, aria }: (typeof KEYS)[number]) => (
    <button
      key={key}
      type="button"
      className={KEY_CLASS}
      aria-label={aria}
      onPointerDown={keepFocus}
      onMouseDown={keepFocus}
      onClick={() => sendKey(key)}
    >
      {Icon ? (
        <Icon className="size-icon-sm" weight="bold" aria-hidden="true" />
      ) : (
        label
      )}
    </button>
  );

  return (
    <div
      className="flex items-center gap-1 px-1 py-1 overflow-x-auto shrink-0"
      role="toolbar"
      aria-label="Terminal keys"
    >
      {KEYS.slice(0, 4).map(renderKey)}
      <button
        type="button"
        className={cn(
          KEY_CLASS,
          ctrlLatched && 'bg-primary text-normal border-info'
        )}
        aria-label="Control (sticky — next letter sends the combo)"
        aria-pressed={ctrlLatched}
        onPointerDown={keepFocus}
        onMouseDown={keepFocus}
        onClick={toggleCtrl}
      >
        ctrl
      </button>
      {KEYS.slice(4).map(renderKey)}
    </div>
  );
}
