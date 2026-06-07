import { useTranslation } from 'react-i18next';
import { ChatsTeardropIcon } from '@phosphor-icons/react';

import { XTermInstance } from '@/shared/components/XTermInstance';

interface CliMainPaneProps {
  workspaceId: string;
  onBackToChat: () => void;
}

/**
 * Terminal-first main pane: hosts the workspace's persistent tmux-backed
 * interactive `claude` session. The tab id is stable per workspace so the
 * terminal instance and WebSocket survive navigation; the tmux session
 * itself survives disconnects and server restarts (see
 * docs/exec-plans/cli-mode-tmux.md).
 */
export function CliMainPane({ workspaceId, onBackToChat }: CliMainPaneProps) {
  const { t } = useTranslation('common');

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
          tabId={`cli-${workspaceId}`}
          workspaceId={workspaceId}
          isActive
          mode="cli"
        />
      </div>
    </div>
  );
}
