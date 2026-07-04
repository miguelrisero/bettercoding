/**
 * Escape sequences for the mobile terminal key bar and gesture layer.
 *
 * Everything here is pure so the sequence choices (especially the
 * application-cursor-keys split and the sticky-Ctrl transform) stay
 * unit-testable without a live terminal.
 */

export type BarKey =
  | 'esc'
  | 'tab'
  | 'shift-tab'
  | 'ctrl-c'
  | 'enter'
  | 'up'
  | 'down'
  | 'left'
  | 'right';

export type ArrowKey = 'up' | 'down' | 'left' | 'right';

const ARROW_FINAL: Record<ArrowKey, string> = {
  up: 'A',
  down: 'B',
  right: 'C',
  left: 'D',
};

/**
 * Sequence for one key-bar key. `applicationCursorKeys` mirrors DECCKM
 * (xterm's `terminal.modes.applicationCursorKeysMode`): full-screen apps like
 * claude/tmux set it and expect SS3 (`ESC O A`) arrows; the normal shell line
 * expects CSI (`ESC [ A`).
 */
export function keySequence(
  key: BarKey,
  applicationCursorKeys: boolean
): string {
  switch (key) {
    case 'esc':
      return '\x1b';
    case 'tab':
      return '\t';
    case 'shift-tab':
      return '\x1b[Z';
    case 'ctrl-c':
      return '\x03';
    case 'enter':
      return '\r';
    default:
      return (applicationCursorKeys ? '\x1bO' : '\x1b[') + ARROW_FINAL[key];
  }
}

/**
 * Control character for a printable key, or null when the combo has no
 * control code (digits, most punctuation). Mirrors what a hardware Ctrl+key
 * produces: letters map case-insensitively (^C = 0x03), plus the classic
 * `@[\]^_?` and space mappings.
 */
export function toCtrlChar(ch: string): string | null {
  if (ch.length !== 1) return null;
  if (ch === ' ') return '\x00';
  if (ch === '?') return '\x7f';
  // ASCII-only: String.toUpperCase would map ß→SS (→ ^S, XOFF freeze!) and
  // other locale surprises, so uppercase by code point arithmetic instead.
  let code = ch.charCodeAt(0);
  if (code >= 0x61 && code <= 0x7a) code -= 0x20; // a-z → A-Z
  // @ A-Z [ \ ] ^ _  →  0x00-0x1f
  if (code >= 0x40 && code <= 0x5f) return String.fromCharCode(code & 0x1f);
  return null;
}

export interface StickyCtrlResult {
  /** Data to forward to the PTY. */
  out: string;
  /** Whether the latch consumed this chunk (transformed it). */
  applied: boolean;
}

/**
 * Apply a latched Ctrl to the next input chunk. Only a single printable
 * character is transformed — multi-char bursts (paste, IME commits, escape
 * sequences) pass through untouched. Either way the caller clears the latch:
 * sticky Ctrl arms exactly one keystroke, like Termius.
 */
export function applyStickyCtrl(data: string): StickyCtrlResult {
  if (data.length === 1) {
    const ctrl = toCtrlChar(data);
    if (ctrl !== null) return { out: ctrl, applied: true };
  }
  return { out: data, applied: false };
}
