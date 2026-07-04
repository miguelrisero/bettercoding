import type { Terminal } from '@xterm/xterm';

import { writeClipboardViaBridge } from './clipboard';
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
 * finished selection is copied exactly once on release — the per-change
 * auto-copy in XTermInstance is suppressed during select mode so a drag
 * doesn't hammer the clipboard with intermediate selections.
 *
 * Listeners are CAPTURE-phase and stop propagation while select mode is on:
 * xterm's own viewport touch handlers (which scroll local scrollback when
 * mouse tracking is off — plain shell terminals) live on child elements and
 * would otherwise scroll the buffer underneath the selection drag.
 */

export interface CellPoint {
  col: number;
  row: number;
}

/**
 * Map a touch point to a 0-based cell within the screen rect (clamped).
 * Returns null for a degenerate rect (hidden/zero-sized pane mid-layout) —
 * dividing by it would produce NaN cells and corrupt xterm's selection.
 */
export function touchToCell(
  clientX: number,
  clientY: number,
  rect: { left: number; top: number; width: number; height: number },
  cols: number,
  rows: number
): CellPoint | null {
  if (rect.width <= 0 || rect.height <= 0 || cols <= 0 || rows <= 0) {
    return null;
  }
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
  let lastSel: { col: number; row: number; length: number } | null = null;

  const cellFromTouch = (t: {
    clientX: number;
    clientY: number;
  }): CellPoint | null => {
    const rect = screen.getBoundingClientRect();
    const cell = touchToCell(
      t.clientX,
      t.clientY,
      rect,
      terminal.cols,
      terminal.rows
    );
    if (!cell) return null;
    // Anchor rows to the ABSOLUTE buffer position at gesture time so the
    // selection targets what the user sees even with scrollback offset.
    return { col: cell.col, row: cell.row + terminal.buffer.active.viewportY };
  };

  const onStart = (e: TouchEvent) => {
    if (!getTerminalMobileState(terminal).selectMode) return;
    // Select mode owns the touch: keep it from xterm's own viewport touch
    // scrolling (child listeners) and from browser default scrolling.
    e.stopPropagation();
    if (e.cancelable) e.preventDefault();
    if (e.touches.length !== 1) {
      anchor = null;
      return;
    }
    anchor = cellFromTouch(e.touches[0]);
    lastSel = null;
    terminal.clearSelection();
  };

  const onMove = (e: TouchEvent) => {
    if (!getTerminalMobileState(terminal).selectMode) return;
    e.stopPropagation();
    if (e.cancelable) e.preventDefault();
    if (!anchor || e.touches.length !== 1) return;
    const focus = cellFromTouch(e.touches[0]);
    if (!focus) return;
    const sel = linearSelection(anchor, focus, terminal.cols);
    // Only touch xterm's selection service when the target cell changed —
    // touchmove fires at 60-120Hz, cells change far less often.
    if (
      lastSel &&
      lastSel.col === sel.col &&
      lastSel.row === sel.row &&
      lastSel.length === sel.length
    ) {
      return;
    }
    lastSel = sel;
    terminal.select(sel.col, sel.row, sel.length);
  };

  const onEnd = () => {
    // Copy exactly once, on release. Bridge-aware helper: in the VSCode
    // iframe navigator.clipboard rejects and the parent handles the copy.
    if (
      anchor !== null &&
      getTerminalMobileState(terminal).selectMode &&
      terminal.hasSelection()
    ) {
      const text = terminal.getSelection();
      if (text) void writeClipboardViaBridge(text);
    }
    anchor = null;
    lastSel = null;
  };

  el.addEventListener('touchstart', onStart, {
    passive: false,
    capture: true,
  });
  el.addEventListener('touchmove', onMove, { passive: false, capture: true });
  el.addEventListener('touchend', onEnd, { passive: true, capture: true });
  el.addEventListener('touchcancel', onEnd, {
    passive: true,
    capture: true,
  });

  return () => {
    el.removeEventListener('touchstart', onStart, { capture: true });
    el.removeEventListener('touchmove', onMove, { capture: true });
    el.removeEventListener('touchend', onEnd, { capture: true });
    el.removeEventListener('touchcancel', onEnd, { capture: true });
  };
}
