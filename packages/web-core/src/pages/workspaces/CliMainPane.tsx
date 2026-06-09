import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { ChatsTeardropIcon } from '@phosphor-icons/react';

import { XTermInstance } from '@/shared/components/XTermInstance';
import { cliTabId, useTerminal } from '@/shared/hooks/useTerminal';

interface CliMainPaneProps {
  workspaceId: string;
  onBackToChat: () => void;
  /**
   * The selected uix session — CLI mode resumes its claude conversation so the
   * terminal joins the exact chat shown in the UI (bidirectional handover).
   */
  sessionId?: string | null;
}

/**
 * Terminal-first main pane: hosts the workspace's persistent tmux-backed
 * interactive `claude` session (see docs/exec-plans/cli-mode-tmux.md).
 *
 * Persistence lives in tmux, NOT in this client: on unmount (navigate away,
 * back to chat) the WebSocket/xterm instance is torn down so hidden
 * workspaces don't accumulate live sockets and server-side PTYs. Remounting
 * reattaches the same tmux session with scrollback intact.
 */
export function CliMainPane({
  workspaceId,
  onBackToChat,
  sessionId,
}: CliMainPaneProps) {
  const { t } = useTranslation('common');
  const { closeTab } = useTerminal();

  useEffect(() => {
    return () => closeTab(workspaceId, cliTabId(workspaceId));
  }, [workspaceId, closeTab]);

  return (
    <div className="h-full bg-secondary flex flex-col">
      <div className="px-4 py-1 flex items-center justify-between shrink-0 h-8">
        <span className="text-sm font-medium text-normal">
          {t('cliMode.title')}
          <span className="ml-2 text-xs text-low">
            {t('cliMode.persistentHint')}
          </span>
        </span>
        <button
          type="button"
          onClick={onBackToChat}
          className="flex items-center gap-1 text-low hover:text-normal transition-colors"
          title={t('cliMode.backToChat')}
        >
          <ChatsTeardropIcon className="size-icon-sm" weight="bold" />
          <span className="text-xs">{t('cliMode.backToChat')}</span>
        </button>
      </div>
      <div className="flex-1 min-h-0 border-t border-border">
        <XTermInstance
          tabId={cliTabId(workspaceId)}
          workspaceId={workspaceId}
          isActive
          mode="cli"
          sessionId={sessionId ?? undefined}
        />
      </div>
    </div>
  );
}
