import type { Terminal } from '@xterm/xterm';

import { isTouchDevice } from '@/shared/hooks/useIsMobile';
import { keySequence } from './terminalKeySequences';
import {
  flashTerminalMobileStatus,
  getTerminalMobileState,
  patchTerminalMobileState,
} from './terminalMobileState';
import { pasteIntoTerminal } from './terminalPaste';
import { installTerminalTouchGestures } from './terminalTouchGestures';
import { installTerminalTouchScroll } from './terminalTouchScroll';
import { installTerminalTouchSelection } from './terminalTouchSelection';

/**
 * Install every touch layer for a freshly created terminal, in the one order
 * that is correct — this function exists to make the ordering invariant
 * impossible to violate from a component:
 *
 *   1. gestures, 2. selection, 3. scroll bridge (LAST)
 *
 * Listener order is attach order, so when a starved long-press promotes to
 * D-pad inside a touchmove, the suppression flag is already set by the time
 * the scroll bridge sees that same event. Attach the bridge first and every
 * starved promotion leaks wheel steps to the PTY.
 *
 * Lifetime: attach once per created terminal; listeners live and die with
 * `terminal.element` (an in-flight D-pad is additionally cancellable via
 * cancelActiveTerminalGesture on React detach). Gestures/selection are gated
 * on touch capability so non-touch sessions carry zero new listeners; the
 * scroll bridge stays unconditional exactly as PR #22 shipped it (inert
 * without touch events).
 */
export function installTerminalTouchLayers(
  terminal: Terminal,
  sendRaw: (data: string) => void
): void {
  if (isTouchDevice()) {
    // A gesture-sent key counts as the ctrl latch's "one keystroke", same as
    // a key-bar tap — otherwise ctrl, D-pad arrow, then a typed letter would
    // surprise-fire a control combo.
    const sendKey = (data: string) => {
      sendRaw(data);
      if (getTerminalMobileState(terminal).ctrlLatched) {
        patchTerminalMobileState(terminal, { ctrlLatched: false });
      }
    };
    installTerminalTouchGestures(terminal, {
      sendArrow: (dir) =>
        sendKey(keySequence(dir, terminal.modes.applicationCursorKeysMode)),
      sendTab: () => sendKey('\t'),
      paste: () =>
        void pasteIntoTerminal(terminal, (message) =>
          flashTerminalMobileStatus(terminal, message)
        ),
    });
    installTerminalTouchSelection(terminal);

    // Keyboard-open ergonomics: focusing the terminal on a touch device pops
    // the system keyboard — jump to the prompt so it isn't hidden in
    // scrollback while the viewport shrinks around it.
    terminal.textarea?.addEventListener('focus', () =>
      terminal.scrollToBottom()
    );
  }
  installTerminalTouchScroll(terminal);
}
