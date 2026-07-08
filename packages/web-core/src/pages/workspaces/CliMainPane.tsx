import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { ChatsTeardropIcon, CircleNotchIcon } from '@phosphor-icons/react';

import { XTermInstance } from '@/shared/components/XTermInstance';
import { LoopAutomationControl } from '@/shared/components/LoopAutomationControl';
import { cliTabId, useTerminal } from '@/shared/hooks/useTerminal';

interface CliMainPaneProps {
  workspaceId: string;
  onBackToChat: () => void;
  /**
   * The selected uix session — CLI mode resumes its claude conversation so the
   * terminal joins the exact chat shown in the UI (bidirectional handover).
   */
  sessionId?: string | null;
  /**
   * Whether this workspace's session list has settled. The terminal (and its
   * WebSocket, which creates the tmux session server-side) is only mounted
   * once this is true: the resume target is baked into the tmux session at
   * creation, so connecting mid-load would hand the bootstrap a missing (or
   * the previous workspace's) session id.
   */
  sessionsReady?: boolean;
  /**
   * An executor process (setup script / coding agent) is running right now.
   * The terminal is held back behind a notice: attaching would bake a
   * premature bootstrap into tmux and (for agent runs) invite working the
   * same conversation twice in chat and CLI. When the run finishes this
   * flips via the workspace stream and the terminal mounts with the proper
   * resume or the parked CLI-first prompt.
   */
  executorRunning?: boolean;
  /**
   * The running process is specifically a coding agent (chat follow-up) as
   * opposed to a setup/cleanup script — switches the gate copy between
   * "claude is working in chat" and "preparing the workspace".
   */
  codingAgentRunning?: boolean;
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
  sessionsReady = true,
  executorRunning = false,
  codingAgentRunning = false,
}: CliMainPaneProps) {
  const { t } = useTranslation('common');
  const { closeTab } = useTerminal();

  useEffect(() => {
    return () => closeTab(workspaceId, cliTabId(workspaceId));
  }, [workspaceId, closeTab]);

  return (
    <div className="h-full bg-secondary flex flex-col">
      <div className="px-4 py-1 flex items-center justify-between gap-2 shrink-0 h-8 min-w-0">
        <span className="text-sm font-medium text-normal min-w-0 truncate">
          <span className="hidden md:inline">{t('cliMode.title')}</span>
          <span className="md:hidden">{t('cliMode.titleShort')}</span>
          <span className="ml-2 text-xs text-low hidden md:inline">
            {t('cliMode.persistentHint')}
          </span>
        </span>
        <div className="flex items-center gap-2 md:gap-3 shrink-0">
          <LoopAutomationControl workspaceId={workspaceId} />
          <button
            type="button"
            onClick={onBackToChat}
            className="flex items-center gap-1 text-low hover:text-normal transition-colors shrink-0"
            title={t('cliMode.backToChat')}
            aria-label={t('cliMode.backToChat')}
          >
            <ChatsTeardropIcon className="size-icon-sm" weight="bold" />
            <span className="text-xs hidden md:inline">
              {t('cliMode.backToChat')}
            </span>
          </button>
        </div>
      </div>
      <div className="flex-1 min-h-0 border-t border-border">
        {executorRunning ? (
          <div
            role="status"
            aria-live="polite"
            className="h-full flex flex-col items-center justify-center gap-3 px-8 text-center"
          >
            <CircleNotchIcon
              className="size-6 animate-spin motion-reduce:animate-none text-low"
              weight="bold"
              aria-hidden="true"
            />
            <p className="text-sm font-medium text-normal">
              {codingAgentRunning
                ? t('cliMode.agentRunningTitle')
                : t('cliMode.setupRunningTitle')}
            </p>
            <p className="text-xs text-low max-w-md">
              {codingAgentRunning
                ? t('cliMode.agentRunningBody')
                : t('cliMode.setupRunningBody')}
            </p>
            {codingAgentRunning && (
              <button
                type="button"
                onClick={onBackToChat}
                className="mt-1 flex items-center gap-1.5 rounded-md border border-border px-3 py-1.5 text-xs text-normal hover:bg-primary transition-colors"
              >
                <ChatsTeardropIcon className="size-icon-sm" weight="bold" />
                {t('cliMode.agentRunningViewChat')}
              </button>
            )}
          </div>
        ) : (
          sessionsReady && (
            <XTermInstance
              tabId={cliTabId(workspaceId)}
              workspaceId={workspaceId}
              isActive
              mode="cli"
              sessionId={sessionId ?? undefined}
            />
          )
        )}
      </div>
    </div>
  );
}
