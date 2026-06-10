import type { ITheme } from '@xterm/xterm';

/**
 * Terminal background, exported so containers can paint their padding ring
 * the same color as the xterm canvas.
 */
export const TERMINAL_BACKGROUND = '#0d0d0d';

/**
 * Fixed dark terminal palette, independent of the app theme.
 *
 * Terminals used to follow the app's light/dark mode, but TUI programs
 * (claude, tmux, anything colored) pick their colors assuming a dark
 * terminal — on the light theme that produced white-on-white text and
 * washed-out panels. One always-dark palette keeps every embedded terminal
 * readable in both app themes; near-black (not pure black) so ANSI black
 * panels drawn by TUIs still read as surfaces.
 */
export function getTerminalTheme(): ITheme {
  return {
    background: TERMINAL_BACKGROUND,
    foreground: '#e6e6e6',
    cursor: '#e6e6e6',
    cursorAccent: TERMINAL_BACKGROUND,
    selectionBackground: '#3d4966',
    selectionForeground: '#e6e6e6',
    black: '#1a1a1a',
    red: '#f7768e',
    green: '#9ece6a',
    yellow: '#e0af68',
    blue: '#7aa2f7',
    magenta: '#bb9af7',
    cyan: '#7dcfff',
    white: '#c0caf5',
    brightBlack: '#545c7e',
    brightRed: '#ff899d',
    brightGreen: '#b9f27c',
    brightYellow: '#f0c674',
    brightBlue: '#8db0ff',
    brightMagenta: '#c9a8fa',
    brightCyan: '#a4daff',
    brightWhite: '#ffffff',
  };
}
