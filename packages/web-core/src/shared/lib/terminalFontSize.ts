/**
 * Mobile terminal font-size preference (A− / A+ steppers in the mobile
 * controls). Desktop keeps the hardcoded default — the preference is only
 * read on touch devices.
 */

export const TERMINAL_DEFAULT_FONT_SIZE = 12;
export const TERMINAL_MIN_FONT_SIZE = 8;
export const TERMINAL_MAX_FONT_SIZE = 20;

const STORAGE_KEY = 'vk-terminal-mobile-font-size';

export function clampTerminalFontSize(size: number): number {
  if (!Number.isFinite(size)) return TERMINAL_DEFAULT_FONT_SIZE;
  return Math.min(
    Math.max(Math.round(size), TERMINAL_MIN_FONT_SIZE),
    TERMINAL_MAX_FONT_SIZE
  );
}

export function loadMobileTerminalFontSize(): number {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (raw === null) return TERMINAL_DEFAULT_FONT_SIZE;
    return clampTerminalFontSize(Number(raw));
  } catch {
    // Storage can be unavailable (private mode, blocked) — use the default.
    return TERMINAL_DEFAULT_FONT_SIZE;
  }
}

export function saveMobileTerminalFontSize(size: number): void {
  try {
    window.localStorage.setItem(
      STORAGE_KEY,
      String(clampTerminalFontSize(size))
    );
  } catch {
    // Best-effort persistence only.
  }
}
