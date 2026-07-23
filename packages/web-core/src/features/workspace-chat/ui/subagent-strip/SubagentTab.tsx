import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { CheckCircleIcon, CpuIcon, XCircleIcon } from '@phosphor-icons/react';

import { RunningDots } from '@vibe/ui/components/RunningDots';
import { cn } from '@vibe/ui/lib/cn';
import type { SubagentDescriptor } from '../../model/subagent-strip-model';

function formatSubagentName(name: string | null, fallback: string): string {
  if (!name) return fallback;
  return name.charAt(0).toUpperCase() + name.slice(1);
}

interface SubagentTabProps {
  descriptor: SubagentDescriptor;
}

export function SubagentTab({ descriptor }: SubagentTabProps) {
  const { t } = useTranslation('common');
  const displayName = useMemo(
    () =>
      formatSubagentName(
        descriptor.name,
        t('conversation.subagent.defaultType')
      ),
    [descriptor.name, t]
  );
  const isActive = descriptor.phase === 'active';
  const toneClass =
    descriptor.phase === 'error'
      ? 'border-error/40 bg-error/5 text-error'
      : descriptor.phase === 'done'
        ? 'border-success/40 bg-success/5 text-normal'
        : 'border-border bg-panel text-normal';

  return (
    <div
      className={cn(
        'flex h-8 min-w-0 max-w-36 flex-1 basis-0 items-center gap-half rounded-sm border px-base',
        toneClass
      )}
      role="status"
      title={descriptor.description}
    >
      <CpuIcon aria-hidden className="size-icon-sm shrink-0 text-low" />
      <span className="min-w-0 truncate text-xs font-medium">
        {displayName}
      </span>
      {isActive ? (
        <>
          <div aria-hidden="true" className="ml-auto">
            <RunningDots />
          </div>
          <span className="sr-only">
            {t('conversation.subagentStrip.working')}
          </span>
        </>
      ) : descriptor.phase === 'error' ? (
        <>
          <XCircleIcon
            aria-hidden
            className="ml-auto size-icon-xs shrink-0 text-error"
            weight="fill"
          />
          <span className="sr-only">
            {t('conversation.subagentStrip.failed')}
          </span>
        </>
      ) : (
        <>
          <CheckCircleIcon
            aria-hidden
            className="ml-auto size-icon-xs shrink-0 text-success"
            weight="fill"
          />
          <span className="sr-only">
            {t('conversation.subagentStrip.completed')}
          </span>
        </>
      )}
    </div>
  );
}
