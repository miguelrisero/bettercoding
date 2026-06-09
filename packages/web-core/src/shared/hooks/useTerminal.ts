import { useContext } from 'react';
import { createHmrContext } from '@/shared/lib/hmrContext';
import type { Terminal } from '@xterm/xterm';
import type { FitAddon } from '@xterm/addon-fit';

export interface TerminalInstance {
  terminal: Terminal;
  fitAddon: FitAddon;
}

export interface TerminalTab {
  id: string;
  title: string;
  workspaceId: string;
  cwd: string;
}

/**
 * Stable terminal id for a workspace's CLI-mode pane.
 *
 * Unlike side-terminal tabs (`generateTabId()` in TerminalProvider), this id
 * intentionally lives ONLY in the provider's connection/instance registries —
 * it is never inserted into `tabsByWorkspace`, so tab-list logic (close-all,
 * active-tab) does not see it. Reducer actions on it are harmless no-ops.
 */
export function cliTabId(workspaceId: string): string {
  return `cli-${workspaceId}`;
}

interface TerminalConnection {
  ws: WebSocket;
  send: (data: string) => void;
  resize: (cols: number, rows: number) => void;
}

export interface TerminalContextType {
  getTabsForWorkspace: (workspaceId: string) => TerminalTab[];
  getActiveTab: (workspaceId: string) => TerminalTab | null;
  createTab: (workspaceId: string, cwd: string) => void;
  closeTab: (workspaceId: string, tabId: string) => void;
  setActiveTab: (workspaceId: string, tabId: string) => void;
  updateTabTitle: (workspaceId: string, tabId: string, title: string) => void;
  clearWorkspaceTabs: (workspaceId: string) => void;
  registerTerminalInstance: (
    tabId: string,
    terminal: Terminal,
    fitAddon: FitAddon
  ) => void;
  getTerminalInstance: (tabId: string) => TerminalInstance | null;
  unregisterTerminalInstance: (tabId: string) => void;
  createTerminalConnection: (
    tabId: string,
    endpoint: string,
    onData: (data: string) => void,
    onExit?: () => void,
    getSize?: () => { cols: number; rows: number } | null
  ) => {
    send: (data: string) => void;
    resize: (cols: number, rows: number) => void;
  };
  getTerminalConnection: (tabId: string) => TerminalConnection | null;
}

export const TerminalContext = createHmrContext<TerminalContextType | null>(
  'TerminalContext',
  null
);

export function useTerminal() {
  const context = useContext(TerminalContext);
  if (!context) {
    throw new Error('useTerminal must be used within TerminalProvider');
  }
  return context;
}
