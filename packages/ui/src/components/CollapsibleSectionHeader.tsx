import type { ReactNode } from 'react';
import { useEffect, useState } from 'react';
import type { Icon } from '@phosphor-icons/react';
import { CaretDownIcon } from '@phosphor-icons/react';
import { cn } from '../lib/cn';

const STORAGE_KEY_PREFIX = 'vibe.ui.collapsible.';

function getInitialExpanded(
  persistKey: string | undefined,
  defaultExpanded: boolean
) {
  if (!persistKey || typeof window === 'undefined') return defaultExpanded;
  try {
    const stored = window.localStorage.getItem(
      `${STORAGE_KEY_PREFIX}${persistKey}`
    );
    if (stored == null) return defaultExpanded;
    return stored === 'true';
  } catch {
    return defaultExpanded;
  }
}

export type SectionAction = {
  icon: Icon;
  onClick: () => void;
  isActive?: boolean;
  ariaLabel?: string;
};

interface CollapsibleSectionHeaderProps {
  persistKey?: string;
  title: string;
  defaultExpanded?: boolean;
  collapsible?: boolean;
  actions?: SectionAction[];
  headerExtra?: ReactNode;
  children?: ReactNode;
  className?: string;
  titleClassName?: string;
}

export function CollapsibleSectionHeader({
  persistKey,
  title,
  defaultExpanded = true,
  collapsible = true,
  actions = [],
  headerExtra,
  children,
  className,
  titleClassName,
}: CollapsibleSectionHeaderProps) {
  const [expanded, setExpanded] = useState(() =>
    getInitialExpanded(persistKey, defaultExpanded)
  );

  useEffect(() => {
    setExpanded(getInitialExpanded(persistKey, defaultExpanded));
  }, [persistKey, defaultExpanded]);

  useEffect(() => {
    if (!persistKey) return;
    try {
      window.localStorage.setItem(
        `${STORAGE_KEY_PREFIX}${persistKey}`,
        String(expanded)
      );
    } catch {
      // Ignore localStorage failures (private mode/quota/security errors).
    }
  }, [persistKey, expanded]);

  const isExpanded = collapsible ? expanded : true;

  return (
    <div className={cn('flex flex-col h-full min-h-0', className)}>
      <div className="flex w-full items-center pl-base">
        {collapsible ? (
          <button
            type="button"
            onClick={() => setExpanded((prev) => !prev)}
            aria-expanded={expanded}
            className="flex min-w-0 flex-1 cursor-pointer items-center justify-between py-half pr-half text-left"
          >
            <span
              className={cn(
                'truncate font-medium tabular-nums text-normal',
                titleClassName
              )}
            >
              {title}
            </span>
            <CaretDownIcon
              weight="fill"
              className={cn(
                'size-icon-xs shrink-0 text-low transition-transform',
                !expanded && '-rotate-90'
              )}
            />
          </button>
        ) : (
          <span
            className={cn(
              'min-w-0 flex-1 truncate py-half font-medium tabular-nums text-normal',
              titleClassName
            )}
          >
            {title}
          </span>
        )}
        <div className="flex shrink-0 items-center gap-half pr-base">
          {headerExtra}
          {actions.map((action, index) => {
            const ActionIcon = action.icon;
            return (
              <button
                key={index}
                type="button"
                onClick={action.onClick}
                aria-label={action.ariaLabel}
                title={action.ariaLabel}
                className={cn(
                  'inline-flex size-8 items-center justify-center hover:text-normal',
                  action.isActive ? 'text-brand' : 'text-low'
                )}
              >
                <ActionIcon className="size-icon-xs" weight="bold" />
              </button>
            );
          })}
        </div>
      </div>
      {isExpanded && children}
    </div>
  );
}
