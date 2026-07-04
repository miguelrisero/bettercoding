import type { Terminal } from '@xterm/xterm';

import { getTerminalMobileState } from './terminalMobileState';

/**
 * Touch drag-selection for select mode.
 *
 * xterm.js has no touch selection at all, and in CLI mode tmux's mouse
 * tracking would eat synthetic mouse events anyway. So select mode does it
 * client-side: map touch coordinates to buffer cells and drive
 * `terminal.select()` directly over the visible buffer. No events are
 * forwarded to the application — tmux never knows a selection happened.
 *
 * Active only while `selectMode` is on (toggled from TerminalMobileControls);
 * the gesture layer and scroll bridge stand down for the same flag. The
 * existing `onSelectionChange` handler auto-copies the result.
 */

export interface CellPoint {
  col: number;
  row: number;
}

/** Map a touch point to a 0-based cell within the screen rect (clamped). */
export function touchToCell(
  clientX: number,
  clientY: number,
  rect: { left: number; top: number; width: number; height: number },
  cols: number,
  rows: number
): CellPoint {
  const cellW = rect.width / cols;
  const cellH = rect.height / rows;
  const col = Math.floor((clientX - rect.left) / cellW);
  const row = Math.floor((clientY - rect.top) / cellH);
  return {
    col: Math.min(Math.max(col, 0), cols - 1),
    row: Math.min(Math.max(row, 0), rows - 1),
  };
}

/**
 * Normalize an anchor→focus drag (absolute buffer rows) into the
 * `terminal.select(column, row, length)` triple. Inclusive of both endpoints.
 */
export function linearSelection(
  anchor: CellPoint,
  focus: CellPoint,
  cols: number
): { col: number; row: number; length: number } {
  const a = anchor.row * cols + anchor.col;
  const b = focus.row * cols + focus.col;
  const start = Math.min(a, b);
  const end = Math.max(a, b);
  return {
    col: start % cols,
    row: Math.floor(start / cols),
    length: end - start + 1,
  };
}

/**
 * Bind select-mode touch handling to a live terminal element. Attach once per
 * created terminal (creation branch); listeners die with the element on
 * dispose. Returns a disposer for tests.
 */
export function installTerminalTouchSelection(terminal: Terminal): () => void {
  const el = terminal.element;
  if (!el) return () => {};
  const screen =
    (el.querySelector('.xterm-screen') as HTMLElement | null) ?? el;

  let anchor: CellPoint | null = null;

  const cellFromTouch = (t: { clientX: number; clientY: number }) => {
    const rect = screen.getBoundingClientRect();
    const cell = touchToCell(
      t.clientX,
      t.clientY,
      rect,
      terminal.cols,
      terminal.rows
    );
    // Anchor rows to the ABSOLUTE buffer position at gesture time so the
    // selection targets what the user sees even with scrollback offset.
    return { col: cell.col, row: cell.row + terminal.buffer.active.viewportY };
  };

  const onStart = (e: TouchEvent) => {
    if (!getTerminalMobileState(terminal).selectMode) return;
    if (e.touches.length !== 1) {
      anchor = null;
      return;
    }
    anchor = cellFromTouch(e.touches[0]);
    terminal.clearSelection();
    // Don't let the tap refocus/scroll — in select mode the finger selects.
    if (e.cancelable) e.preventDefault();
  };

  const onMove = (e: TouchEvent) => {
    if (!anchor || !getTerminalMobileState(terminal).selectMode) return;
    if (e.touches.length !== 1) return;
    const focus = cellFromTouch(e.touches[0]);
    const sel = linearSelection(anchor, focus, terminal.cols);
    terminal.select(sel.col, sel.row, sel.length);
    if (e.cancelable) e.preventDefault();
  };

  const onEnd = () => {
    // Copy exactly once, on release — the per-change auto-copy is suppressed
    // in select mode so a drag doesn't overwrite the clipboard dozens of
    // times with intermediate selections.
    if (
      anchor !== null &&
      getTerminalMobileState(terminal).selectMode &&
      terminal.hasSelection()
    ) {
      const text = terminal.getSelection();
      if (text) {
        void navigator.clipboard?.writeText(text).catch(() => {
          // Clipboard can be blocked; the selection itself still stands and
          // the Copy button reports errors explicitly.
        });
      }
    }
    anchor = null;
  };

  el.addEventListener('touchstart', onStart, { passive: false });
  el.addEventListener('touchmove', onMove, { passive: false });
  el.addEventListener('touchend', onEnd, { passive: true });
  el.addEventListener('touchcancel', onEnd, { passive: true });

  return () => {
    el.removeEventListener('touchstart', onStart);
    el.removeEventListener('touchmove', onMove);
    el.removeEventListener('touchend', onEnd);
    el.removeEventListener('touchcancel', onEnd);
  };
}
