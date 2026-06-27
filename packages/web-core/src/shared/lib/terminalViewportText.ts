import type { IBuffer } from '@xterm/xterm';

/**
 * Reconstruct the text of the visible viewport, preserving logical lines across
 * xterm's hard-wrapped rows.
 *
 * `IBufferLine.translateToString()` does not account for wrapping, so naively
 * joining every visual row with "\n" corrupts wrapped output. xterm marks a row
 * as a wrap-continuation of the previous one via `IBufferLine.isWrapped`, so we
 * only insert a newline when the NEXT row is not a continuation of this one.
 *
 * Tolerates missing/blank lines, wide characters, and the alternate buffer.
 */
export function extractViewportText(
  buffer: Pick<IBuffer, 'getLine'>,
  viewportY: number,
  rows: number
): string {
  const parts: string[] = [];
  for (let i = 0; i < rows; i++) {
    const line = buffer.getLine(viewportY + i);
    if (!line) continue;
    const continued = buffer.getLine(viewportY + i + 1)?.isWrapped ?? false;
    // Don't trim a wrapped row — its content runs straight into the next row.
    parts.push(line.translateToString(!continued));
    if (!continued) parts.push('\n');
  }
  return parts.join('').replace(/\n+$/, '');
}
