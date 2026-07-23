import { useId, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { BaseCodingAgent } from 'shared/types';
import { CaretDownIcon, CpuIcon, UserCircleIcon } from '@phosphor-icons/react';

import { RunningDots } from '@vibe/ui/components/RunningDots';
import { cn } from '@vibe/ui/lib/cn';
import { AgentIcon } from '@/shared/components/AgentIcon';
import { useIsMobile } from '@/shared/hooks/useIsMobile';
import { useSubagentStrip } from '../../model/hooks/useSubagentStrip';
import type {
  StripTab,
  SubagentDescriptor,
} from '../../model/subagent-strip-model';
import { SubagentDetailList, SubagentOverflow } from './SubagentDrawer';
import { SubagentTab } from './SubagentTab';

interface SubagentStripProps {
  executor?: BaseCodingAgent | null;
  workspaceId?: string;
  sessionId?: string;
}

interface StripViewProps extends SubagentStripProps {
  tabs: StripTab[];
  drawer: SubagentDescriptor[];
  overflowCount: number;
  overflowLabelMode: 'more' | 'done';
  activeCount: number;
  doneCount: number;
}

function MainChip({ executor }: Pick<SubagentStripProps, 'executor'>) {
  const { t } = useTranslation('common');

  return (
    <div className="flex h-8 shrink-0 items-center gap-half rounded-sm bg-secondary px-base text-xs font-medium text-normal">
      {executor ? (
        <AgentIcon agent={executor} className="size-icon-sm shrink-0" />
      ) : (
        <UserCircleIcon
          aria-hidden
          className="size-icon-sm shrink-0 text-low"
        />
      )}
      <span>{t('conversation.subagentStrip.main')}</span>
    </div>
  );
}

function DesktopSubagentStrip({
  executor,
  workspaceId,
  sessionId,
  tabs,
  drawer,
  overflowCount,
  overflowLabelMode,
}: StripViewProps) {
  const { t } = useTranslation('common');

  return (
    <section
      className="shrink-0 border-b border-border bg-primary/95 px-double py-half backdrop-blur-sm"
      aria-label={t('conversation.subagentStrip.allSubagents')}
    >
      <div className="flex min-w-0 items-center gap-half">
        <MainChip executor={executor} />
        <div className="flex min-w-0 flex-1 items-center gap-half overflow-hidden">
          {tabs.map(({ descriptor }) => (
            <SubagentTab key={descriptor.key} descriptor={descriptor} />
          ))}
        </div>
        {overflowCount > 0 ? (
          <SubagentOverflow
            descriptors={drawer}
            count={overflowCount}
            labelMode={overflowLabelMode}
            workspaceId={workspaceId}
            sessionId={sessionId}
          />
        ) : null}
      </div>
    </section>
  );
}

function MobileActiveSubagent({
  descriptor,
}: {
  descriptor: SubagentDescriptor;
}) {
  const { t } = useTranslation('common');
  const name = descriptor.name
    ? descriptor.name.charAt(0).toUpperCase() + descriptor.name.slice(1)
    : t('conversation.subagent.defaultType');

  return (
    <div className="flex items-center gap-base rounded-sm border border-border bg-panel px-double py-base">
      <CpuIcon aria-hidden className="size-icon-base shrink-0 text-low" />
      <div className="min-w-0 flex-1">
        <span className="block truncate text-xs font-medium uppercase tracking-wide text-low">
          {name}
        </span>
        <span className="block truncate text-sm text-normal">
          {descriptor.description}
        </span>
      </div>
      <div aria-hidden="true" className="shrink-0">
        <RunningDots />
      </div>
      <span className="sr-only">{t('conversation.subagentStrip.working')}</span>
    </div>
  );
}

function MobileSubagentStrip({
  workspaceId,
  sessionId,
  tabs,
  drawer,
  activeCount,
  doneCount,
}: StripViewProps) {
  const { t } = useTranslation('common');
  const [open, setOpen] = useState(false);
  const panelId = useId();
  const descriptors = useMemo(
    () => [...tabs.map(({ descriptor }) => descriptor), ...drawer],
    [drawer, tabs]
  );
  const active = descriptors.filter(
    (descriptor) => descriptor.phase === 'active'
  );
  const finished = descriptors.filter(
    (descriptor) => descriptor.phase !== 'active'
  );

  return (
    <section className="shrink-0 border-b border-border bg-primary/95 backdrop-blur-sm">
      <button
        type="button"
        className="flex min-h-11 w-full items-center justify-between gap-base px-double text-sm font-medium tabular-nums text-normal transition-colors hover:bg-secondary/60 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-brand"
        onClick={() => {
          setOpen((current) => !current);
        }}
        aria-controls={panelId}
        aria-expanded={open}
      >
        <span>
          {t('conversation.subagentStrip.mobileSummary', {
            active: activeCount,
            done: doneCount,
          })}
        </span>
        <CaretDownIcon
          aria-hidden
          className={cn(
            'size-icon-sm shrink-0 transition-transform duration-150',
            open ? 'rotate-180' : 'rotate-0'
          )}
        />
      </button>
      {open ? (
        <div
          id={panelId}
          className="max-h-[min(50vh,24rem)] overflow-y-auto border-t border-border bg-primary p-double"
          role="region"
          aria-label={t('conversation.subagentStrip.allSubagents')}
        >
          <div className="flex flex-col gap-double">
            {active.length > 0 ? (
              <div className="flex flex-col gap-base">
                <h3 className="text-xs font-medium uppercase tracking-wide text-low">
                  {t('conversation.subagentStrip.stillRunning')}
                </h3>
                {active.map((descriptor) => (
                  <MobileActiveSubagent
                    key={descriptor.key}
                    descriptor={descriptor}
                  />
                ))}
              </div>
            ) : null}
            {finished.length > 0 ? (
              <div className="flex flex-col gap-base">
                <h3 className="text-xs font-medium uppercase tracking-wide text-low">
                  {t('conversation.subagentStrip.drawerTitle')}
                </h3>
                <SubagentDetailList
                  descriptors={finished}
                  workspaceId={workspaceId}
                  sessionId={sessionId}
                />
              </div>
            ) : null}
          </div>
        </div>
      ) : null}
    </section>
  );
}

export function SubagentStrip({
  executor,
  workspaceId,
  sessionId,
}: SubagentStripProps) {
  const strip = useSubagentStrip();
  const isMobile = useIsMobile();

  if (!strip.hasAny) return null;

  const props: StripViewProps = {
    executor,
    workspaceId,
    sessionId,
    tabs: strip.tabs,
    drawer: strip.drawer,
    overflowCount: strip.overflowCount,
    overflowLabelMode: strip.overflowLabelMode,
    activeCount: strip.activeCount,
    doneCount: strip.doneCount,
  };

  return isMobile ? (
    <MobileSubagentStrip {...props} />
  ) : (
    <DesktopSubagentStrip {...props} />
  );
}
