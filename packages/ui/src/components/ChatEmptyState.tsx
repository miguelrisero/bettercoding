import { ChatCircleDotsIcon } from '@phosphor-icons/react';
import type { ReactNode } from 'react';

import { cn } from '../lib/cn';

interface ChatEmptyStateProps {
  title: string;
  description?: string;
  className?: string;
  /** Optional leading icon override (defaults to a chat bubble). */
  icon?: ReactNode;
  /** Optional primary action rendered below the description. */
  actionLabel?: string;
  onAction?: () => void;
}

export function ChatEmptyState({
  title,
  description,
  className,
  icon,
  actionLabel,
  onAction,
}: ChatEmptyStateProps) {
  return (
    <div
      className={cn(
        'mx-auto flex max-w-md flex-col items-center gap-2 text-center',
        className
      )}
    >
      <div className="flex size-12 items-center justify-center rounded-full border border-border/70 bg-panel text-low">
        {icon ?? <ChatCircleDotsIcon className="size-6" />}
      </div>
      <p className="text-sm font-medium text-normal">{title}</p>
      {description ? <p className="text-sm text-low">{description}</p> : null}
      {actionLabel && onAction ? (
        <button
          type="button"
          onClick={onAction}
          className="mt-2 inline-flex items-center gap-1.5 rounded-md border border-border px-3 py-1.5 text-xs font-medium text-normal transition-colors hover:bg-primary"
        >
          {actionLabel}
        </button>
      ) : null}
    </div>
  );
}
