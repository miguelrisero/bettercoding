import {
  CheckCircleIcon,
  InfoIcon,
  WarningCircleIcon,
  XCircleIcon,
  XIcon,
} from '@phosphor-icons/react';

import { cn } from '../lib/cn';

export type ToastTone = 'info' | 'success' | 'warning' | 'error';

export interface ToastProps {
  message: string;
  tone?: ToastTone;
  onDismiss?: () => void;
  dismissLabel?: string;
  className?: string;
}

const toneStyles: Record<ToastTone, string> = {
  info: 'text-normal',
  success: 'text-success',
  warning: 'text-warning',
  error: 'text-error',
};

const toneIcons = {
  info: InfoIcon,
  success: CheckCircleIcon,
  warning: WarningCircleIcon,
  error: XCircleIcon,
} satisfies Record<ToastTone, typeof InfoIcon>;

/** Controlled, reusable status toast for local app surfaces. */
export function Toast({
  message,
  tone = 'info',
  onDismiss,
  dismissLabel = 'Dismiss notification',
  className,
}: ToastProps) {
  const StatusIcon = toneIcons[tone];
  const isError = tone === 'error';

  return (
    <div
      role={isError ? 'alert' : 'status'}
      aria-live={isError ? 'assertive' : 'polite'}
      className={cn(
        'flex min-h-10 w-max max-w-[min(32rem,calc(100vw-2rem))] items-center gap-base rounded-md border border-border bg-panel px-base py-half text-sm text-pretty shadow-lg',
        'animate-in fade-in-0 slide-in-from-bottom-1 duration-150 motion-reduce:animate-none',
        toneStyles[tone],
        className
      )}
    >
      <StatusIcon className="size-icon-base shrink-0" aria-hidden="true" />
      <span className="min-w-0 flex-1 text-normal">{message}</span>
      {onDismiss && (
        <button
          type="button"
          onClick={onDismiss}
          aria-label={dismissLabel}
          className="-my-half -mr-half flex size-10 shrink-0 items-center justify-center rounded-sm text-low transition-colors hover:bg-secondary hover:text-normal focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-brand"
        >
          <XIcon className="size-icon-sm" weight="bold" aria-hidden="true" />
        </button>
      )}
    </div>
  );
}
