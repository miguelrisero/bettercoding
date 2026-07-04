import type { Terminal } from '@xterm/xterm';

// Terminals with a paste currently flowing through their onData pipe.
// xterm's paste() emits data synchronously, so the marker brackets exactly
// the paste's chunks — the sticky-Ctrl transform checks it to keep clipboard
// text from ever being turned into control codes (a 1-char unbracketed paste
// is indistinguishable from a keystroke by shape alone).
const pasting = new WeakSet<Terminal>();

export function isTerminalPasting(terminal: Terminal): boolean {
  return pasting.has(terminal);
}

/** The ONLY way app code should paste into a terminal — carries the marker. */
export function pasteTextIntoTerminal(terminal: Terminal, text: string): void {
  pasting.add(terminal);
  try {
    terminal.paste(text);
  } finally {
    pasting.delete(terminal);
  }
}

/**
 * Guarded clipboard→terminal paste with explicit user feedback — the single
 * implementation behind both the Paste button and the three-finger gesture.
 * A silent clipboard read would be a pastejacking aid, so every outcome
 * reports: unavailable (no Clipboard API — insecure context/WebView), empty,
 * blocked (permission denied), or pasted.
 */
export async function pasteIntoTerminal(
  terminal: Terminal,
  notify: (message: string) => void
): Promise<void> {
  // Optional chaining alone would resolve to undefined on insecure contexts
  // and look like an empty clipboard — report "unavailable" distinctly.
  if (!navigator.clipboard?.readText) {
    notify('Paste unavailable');
    return;
  }
  try {
    const text = await navigator.clipboard.readText();
    if (!text) {
      notify('Clipboard empty');
      return;
    }
    pasteTextIntoTerminal(terminal, text);
    notify('Pasted');
  } catch {
    notify('Paste blocked');
  }
}
