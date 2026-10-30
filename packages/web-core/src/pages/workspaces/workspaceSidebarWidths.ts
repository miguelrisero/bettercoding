export type SidebarId = 'left' | 'right';
export const SIDEBAR_MIN_WIDTH = 200;
export const SIDEBAR_DEFAULT_WIDTH = 300;
export const SIDEBAR_MAX_WIDTH = 720;
export const WORKSPACE_CENTER_MIN_WIDTH = 320;

const keys: Record<SidebarId, string> = {
  left: 'bettercoding.workspace-sidebar.left.pixels',
  right: 'bettercoding.workspace-sidebar.right.pixels',
};

export function clampSidebarWidth(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value)
    ? Math.min(SIDEBAR_MAX_WIDTH, Math.max(SIDEBAR_MIN_WIDTH, value))
    : SIDEBAR_DEFAULT_WIDTH;
}

export function readSidebarWidth(sidebar: SidebarId): number {
  try {
    const value = localStorage.getItem(keys[sidebar]);
    return value === null || value.trim() === ''
      ? SIDEBAR_DEFAULT_WIDTH
      : clampSidebarWidth(Number(value));
  } catch {
    return SIDEBAR_DEFAULT_WIDTH;
  }
}

export function writeSidebarWidth(sidebar: SidebarId, width: number): number {
  const applied = clampSidebarWidth(width);
  try {
    localStorage.setItem(keys[sidebar], String(applied));
  } catch {
    // Keep resizing functional when browser storage is unavailable.
  }
  return applied;
}
