import { useEffect, useState } from 'react';
import { CaretDownIcon, TerminalWindowIcon } from '@phosphor-icons/react';
import { useTranslation } from 'react-i18next';
import type { UnassignedCliSession } from 'shared/types';

import { cn } from '@/shared/lib/utils';
import { Alert, AlertDescription } from '@vibe/ui/components/Alert';
import { Badge } from '@vibe/ui/components/Badge';
import { Button } from '@vibe/ui/components/Button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@vibe/ui/components/Dialog';
import { partitionCliSessionsByKind } from '../model/partitionCliSessionsByKind';

interface UnassignedCliSessionsProps {
  sessions: UnassignedCliSession[];
  assigningSessionId?: string;
  error: Error | null;
  onAssign: (claudeSessionId: string) => Promise<void>;
}

export function UnassignedCliSessions({
  sessions,
  assigningSessionId,
  error,
  onAssign,
}: UnassignedCliSessionsProps) {
  const { t } = useTranslation('common');
  const [open, setOpen] = useState(false);
  const [showAgents, setShowAgents] = useState(false);
  const { main, agents } = partitionCliSessionsByKind(sessions);

  useEffect(() => {
    if (sessions.length === 0) setOpen(false);
  }, [sessions.length]);

  useEffect(() => {
    if (!open) setShowAgents(false);
  }, [open]);

  if (sessions.length === 0) return null;

  const renderSession = (session: UnassignedCliSession) => {
    const isAssigning = assigningSessionId === session.claude_session_id;
    return (
      <div
        key={session.claude_session_id}
        className="rounded-sm border border-border bg-secondary/20 p-double"
      >
        <div className="flex flex-col gap-double sm:flex-row sm:items-start">
          <div className="min-w-0 flex-1 space-y-half">
            <Badge
              variant={session.kind === 'subagent' ? 'outline' : 'secondary'}
              className="mb-half font-normal"
            >
              {t(
                session.kind === 'subagent'
                  ? 'conversation.quarantine.agentBadge'
                  : 'conversation.quarantine.mainBadge'
              )}
            </Badge>
            <p className="line-clamp-2 text-sm text-normal">
              {session.first_prompt_snippet ??
                t('conversation.quarantine.noPreview')}
            </p>
            <p className="truncate text-xs text-low" title={session.cwd}>
              {session.cwd}
            </p>
            <code
              className="block truncate text-xs text-low"
              title={session.claude_session_id}
            >
              {session.claude_session_id}
            </code>
          </div>
          <Button
            type="button"
            size="sm"
            variant="outline"
            className="shrink-0"
            disabled={Boolean(assigningSessionId)}
            onClick={() => {
              void onAssign(session.claude_session_id).catch(() => {});
            }}
          >
            {isAssigning
              ? t('conversation.quarantine.assigning')
              : t('conversation.quarantine.assign')}
          </Button>
        </div>
      </div>
    );
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <Button
        type="button"
        size="xs"
        variant="outline"
        className="gap-base border-border bg-primary/90 text-normal shadow-sm backdrop-blur-sm"
        onClick={() => setOpen(true)}
        aria-label={t('conversation.quarantine.openLabel', {
          count: sessions.length,
        })}
      >
        <TerminalWindowIcon className="size-icon-xs" aria-hidden="true" />
        <span>{t('conversation.quarantine.affordance')}</span>
        <Badge
          variant="secondary"
          className="min-w-5 justify-center px-half py-0 font-normal"
        >
          {sessions.length}
        </Badge>
      </Button>

      <DialogContent className="max-h-[85vh] overflow-hidden sm:max-w-xl">
        <DialogHeader className="pr-double">
          <DialogTitle>{t('conversation.quarantine.title')}</DialogTitle>
          <DialogDescription>
            {t('conversation.quarantine.description')}
          </DialogDescription>
        </DialogHeader>

        {error && (
          <Alert variant="destructive" className="mt-double">
            <AlertDescription>
              {t('conversation.quarantine.error')}
            </AlertDescription>
          </Alert>
        )}

        <div className="mt-double max-h-[60vh] space-y-base overflow-y-auto pr-half">
          {main.map(renderSession)}
          {agents.length > 0 && (
            <div className="space-y-base">
              <button
                type="button"
                onClick={() => setShowAgents((value) => !value)}
                className="flex w-full items-center gap-half text-xs text-low"
              >
                <CaretDownIcon
                  className={cn(
                    'size-3 transition-transform',
                    !showAgents && '-rotate-90'
                  )}
                  aria-hidden="true"
                />
                <span>
                  {t(
                    showAgents
                      ? 'conversation.quarantine.hideBackgroundAgents'
                      : 'conversation.quarantine.showBackgroundAgents',
                    { count: agents.length }
                  )}
                </span>
              </button>
              {showAgents && agents.map(renderSession)}
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
