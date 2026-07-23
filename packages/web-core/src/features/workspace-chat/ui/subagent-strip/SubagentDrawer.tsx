import { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { CaretDownIcon } from '@phosphor-icons/react';

import {
  ChatSubagentEntry,
  type ChatSubagentEntryRenderProps,
} from '@vibe/ui/components/ChatSubagentEntry';
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@vibe/ui/components/Popover';
import { cn } from '@vibe/ui/lib/cn';
import type { SubagentDescriptor } from '../../model/subagent-strip-model';
import { AppChatMarkdown } from '../DisplayConversationEntry';

interface SubagentDetailListProps {
  descriptors: SubagentDescriptor[];
  workspaceId?: string;
  sessionId?: string;
}

export function SubagentDetailList({
  descriptors,
  workspaceId,
  sessionId,
}: SubagentDetailListProps) {
  const { t } = useTranslation('common');
  const [expandedKeys, setExpandedKeys] = useState<Set<string>>(
    () => new Set()
  );
  const toggleExpanded = useCallback((key: string) => {
    setExpandedKeys((current) => {
      const next = new Set(current);
      if (next.has(key)) {
        next.delete(key);
      } else {
        next.add(key);
      }
      return next;
    });
  }, []);
  const renderMarkdown = useCallback(
    ({
      content,
      workspaceId: renderedWorkspaceId,
    }: ChatSubagentEntryRenderProps) => (
      <AppChatMarkdown
        content={content}
        workspaceId={renderedWorkspaceId}
        sessionId={sessionId}
        className={undefined}
        maxWidth={undefined}
      />
    ),
    [sessionId]
  );

  return (
    <div className="flex flex-col gap-base">
      {descriptors.map((descriptor) => {
        const hasResult = Boolean(descriptor.result?.value);
        const statusLabel =
          descriptor.phase === 'active'
            ? t('conversation.subagentStrip.stillRunning')
            : descriptor.phase === 'done'
              ? t('conversation.subagentStrip.completed')
              : t('conversation.subagentStrip.failed');

        return (
          <div key={descriptor.key}>
            <ChatSubagentEntry
              description={descriptor.description}
              subagentType={descriptor.name}
              result={descriptor.result}
              expanded={expandedKeys.has(descriptor.key)}
              onToggle={
                hasResult
                  ? () => {
                      toggleExpanded(descriptor.key);
                    }
                  : undefined
              }
              status={descriptor.status}
              workspaceId={workspaceId}
              renderMarkdown={renderMarkdown}
            />
            <span className="sr-only">{statusLabel}</span>
          </div>
        );
      })}
    </div>
  );
}

interface SubagentOverflowProps extends SubagentDetailListProps {
  count: number;
  labelMode: 'more' | 'done';
}

export function SubagentOverflow({
  descriptors,
  count,
  labelMode,
  workspaceId,
  sessionId,
}: SubagentOverflowProps) {
  const { t } = useTranslation('common');
  const [open, setOpen] = useState(false);
  const label =
    labelMode === 'more'
      ? t('conversation.subagentStrip.more', { count })
      : t('conversation.subagentStrip.done', { count });
  const title =
    labelMode === 'more'
      ? t('conversation.subagentStrip.allSubagents')
      : t('conversation.subagentStrip.drawerTitle');

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="flex min-h-10 shrink-0 items-center gap-half rounded-sm px-base text-xs font-medium tabular-nums text-low transition-colors hover:bg-secondary hover:text-normal focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-brand"
          aria-expanded={open}
        >
          {label}
          <CaretDownIcon
            aria-hidden
            className={cn(
              'size-icon-xs shrink-0 transition-transform duration-150',
              open ? 'rotate-180' : 'rotate-0'
            )}
          />
        </button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        side="bottom"
        className="flex max-h-[min(75vh,var(--radix-popover-content-available-height))] flex-col gap-base"
      >
        <h3 className="shrink-0 text-sm font-medium text-normal">{title}</h3>
        <div className="min-h-0 overflow-y-auto">
          <SubagentDetailList
            descriptors={descriptors}
            workspaceId={workspaceId}
            sessionId={sessionId}
          />
        </div>
      </PopoverContent>
    </Popover>
  );
}
