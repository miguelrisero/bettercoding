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
  useTerminalMobileState,
} from '@/shared/lib/terminalMobileState';

interface TerminalKeyBarProps {
  /** Live terminal (null until mounted) — used for cursor-mode + ctrl latch state. */
  terminal: Terminal | null;
  /** Send raw bytes to the PTY (bypasses the sticky-Ctrl transform on typed input). */
  onSendKey: (data: string) => void;
}

// Icons instead of key-glyph characters (⇥ ⇧⇥ ⏎): those codepoints are
// missing from Android/Linux system fonts and render as tofu boxes; the
// phosphor icons render identically everywhere. 'ctrl' is the latching
// modifier button, rendered specially at its position in this list.
const KEYS: ReadonlyArray<{
  key: BarKey | 'ctrl';
  label?: string;
  Icon?: Icon;
  aria: string;
}> = [
  { key: 'esc', label: 'esc', aria: 'Escape' },
  { key: 'tab', Icon: ArrowLineRightIcon, aria: 'Tab' },
  { key: 'shift-tab', Icon: ArrowLineLeftIcon, aria: 'Shift+Tab' },
  { key: 'ctrl-c', label: '^C', aria: 'Control+C (interrupt)' },
  {
    key: 'ctrl',
    label: 'ctrl',
    aria: 'Control (sticky — next letter sends the combo)',
  },
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
  const { ctrlLatched } = useTerminalMobileState(terminal);

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

  const renderKey = ({ key, label, Icon, aria }: (typeof KEYS)[number]) => {
    const isCtrl = key === 'ctrl';
    return (
      <button
        key={key}
        type="button"
        className={cn(
          KEY_CLASS,
          isCtrl && ctrlLatched && 'bg-primary text-normal border-info'
        )}
        aria-label={aria}
        aria-pressed={isCtrl ? ctrlLatched : undefined}
        onClick={isCtrl ? toggleCtrl : () => sendKey(key)}
      >
        {Icon ? (
          <Icon className="size-icon-sm" weight="bold" aria-hidden="true" />
        ) : (
          label
        )}
      </button>
    );
  };

  return (
    // preventDefault on pointer/mouse down at the toolbar level (events
    // bubble) so no tap steals focus from xterm's textarea — the system
    // keyboard stays open while sending keys.
    // role="group", not "toolbar": the ARIA toolbar contract requires roving
    // tabindex + arrow-key traversal, and arrow keys here are CONTENT.
    <div
      className="flex items-center gap-1 px-1 py-1 overflow-x-auto shrink-0"
      role="group"
      aria-label="Terminal keys"
      onPointerDown={keepFocus}
      onMouseDown={keepFocus}
    >
      {KEYS.map(renderKey)}
    </div>
  );
}
