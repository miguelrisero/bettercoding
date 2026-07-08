import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  ArrowsClockwiseIcon,
  ClockIcon,
  GearSixIcon,
  MoonIcon,
  XIcon,
} from '@phosphor-icons/react';

import { Switch } from '@vibe/ui/components/Switch';
import { Button } from '@vibe/ui/components/Button';
import { IconButton } from '@vibe/ui/components/IconButton';
import { Tooltip } from '@vibe/ui/components/Tooltip';
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@vibe/ui/components/Popover';
import { useLoopAutomation } from '@/shared/hooks/useLoopAutomation';

interface LoopAutomationControlProps {
  workspaceId: string;
}

// Applied locally only until the backend persists a policy (toggling on with no
// existing policy sends nulls so the server fills its own defaults).
const DEFAULT_INTERVAL_MINUTES = 5;
const DEFAULT_MAX_ATTEMPTS = 10;

const fieldClassName =
  'w-full px-2 py-1 bg-secondary rounded-sm border border-border text-xs ' +
  'text-normal placeholder:text-low focus:outline-none focus:ring-1 ' +
  'focus:ring-brand';

function formatLocalTime(iso: string): string {
  return new Date(iso).toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
  });
}

function formatUtcTime(iso: string): string {
  return new Date(iso).toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
    timeZone: 'UTC',
  });
}

/**
 * Compact "Keep going" loop-automation control for the CLI pane header.
 *
 * Surfaces a toggle (default OFF), a settings popover (retry interval,
 * continuation prompt, max attempts) and a status line for the next scheduled
 * wakeup. All persistence flows through the workspace's loop-automation policy.
 */
export function LoopAutomationControl({
  workspaceId,
}: LoopAutomationControlProps) {
  const { t } = useTranslation('common');
  const { status, updatePolicy, deleteWakeup } = useLoopAutomation(workspaceId);

  const policy = status?.policy ?? null;
  const enabled = policy?.enabled ?? false;

  const [open, setOpen] = useState(false);
  const [intervalMinutes, setIntervalMinutes] = useState(
    String(DEFAULT_INTERVAL_MINUTES)
  );
  const [continuationPrompt, setContinuationPrompt] = useState('');
  const [maxAttempts, setMaxAttempts] = useState(String(DEFAULT_MAX_ATTEMPTS));

  // Re-seed the form from the saved policy whenever the popover opens (or the
  // policy changes underneath it) so it always reflects persisted values.
  useEffect(() => {
    if (!open) return;
    setIntervalMinutes(
      policy
        ? String(Math.round(Number(policy.retry_interval_secs) / 60))
        : String(DEFAULT_INTERVAL_MINUTES)
    );
    setContinuationPrompt(policy?.continuation_prompt ?? '');
    setMaxAttempts(
      policy
        ? String(Number(policy.max_attempts))
        : String(DEFAULT_MAX_ATTEMPTS)
    );
  }, [open, policy]);

  const nextWakeup = useMemo(() => {
    const pending = status?.pending_wakeups ?? [];
    if (pending.length === 0) return null;
    return [...pending].sort(
      (a, b) => Date.parse(a.fire_at) - Date.parse(b.fire_at)
    )[0];
  }, [status?.pending_wakeups]);

  const handleToggle = (next: boolean) => {
    updatePolicy.mutate({
      enabled: next,
      retryIntervalSecs: policy ? Number(policy.retry_interval_secs) : null,
      continuationPrompt: policy ? policy.continuation_prompt : null,
      maxAttempts: policy ? Number(policy.max_attempts) : null,
    });
  };

  const handleSave = () => {
    const minutes = Math.max(
      1,
      Math.round(Number(intervalMinutes) || DEFAULT_INTERVAL_MINUTES)
    );
    const attempts = Math.max(0, Math.round(Number(maxAttempts) || 0));
    updatePolicy.mutate(
      {
        enabled: true,
        retryIntervalSecs: minutes * 60,
        continuationPrompt: continuationPrompt.trim()
          ? continuationPrompt
          : null,
        maxAttempts: attempts,
      },
      { onSuccess: () => setOpen(false) }
    );
  };

  const maxAttemptsCount = policy ? Number(policy.max_attempts) : 0;
  const attemptsUsed = policy ? Number(policy.attempts_used) : 0;
  const showAttempts = enabled && maxAttemptsCount > 0;

  // Mobile (<768px) shows only the bare time next to the moon/clock icon; the
  // full localized status stays in the accessibility tree via sr-only. md:
  // (>=768px) is the exact complement of useIsMobile's 767px max-width — keep
  // in sync with MOBILE_BREAKPOINT in useIsMobile.ts.
  const isUsageLimitWake = nextWakeup?.kind === 'usage_limit_wake';
  const wakeupTimeText = nextWakeup
    ? isUsageLimitWake
      ? formatUtcTime(nextWakeup.fire_at)
      : formatLocalTime(nextWakeup.fire_at)
    : null;
  const wakeupStatusText = nextWakeup
    ? isUsageLimitWake
      ? t('loopAutomation.wakingAt', { time: wakeupTimeText })
      : t('loopAutomation.retryingAt', { time: wakeupTimeText })
    : null;

  return (
    <div className="flex items-center gap-1.5 md:gap-2 min-w-0">
      {enabled && nextWakeup && (
        <span className="flex items-center gap-1 text-xs text-low whitespace-nowrap shrink-0">
          {isUsageLimitWake ? (
            <MoonIcon className="size-icon-sm" weight="bold" aria-hidden />
          ) : (
            <ClockIcon className="size-icon-sm" weight="bold" aria-hidden />
          )}
          <span className="sr-only md:not-sr-only">{wakeupStatusText}</span>
          <span className="md:hidden" aria-hidden>
            {wakeupTimeText}
          </span>
          {showAttempts && (
            <span className="hidden md:inline">
              {'· '}
              {t('loopAutomation.attempts', {
                used: attemptsUsed,
                max: maxAttemptsCount,
              })}
            </span>
          )}
          <IconButton
            icon={XIcon}
            aria-label={t('loopAutomation.cancelWakeup')}
            title={t('loopAutomation.cancelWakeup')}
            onClick={() => deleteWakeup.mutate(nextWakeup.id)}
            disabled={deleteWakeup.isPending}
          />
        </span>
      )}

      {showAttempts && !nextWakeup && (
        <span className="text-xs text-low hidden md:inline">
          {t('loopAutomation.attempts', {
            used: attemptsUsed,
            max: maxAttemptsCount,
          })}
        </span>
      )}

      <Tooltip content={t('loopAutomation.tooltip')} side="bottom">
        <span className="flex items-center gap-1.5 shrink-0">
          <ArrowsClockwiseIcon
            className="size-icon-sm text-low"
            weight="bold"
            aria-hidden
          />
          <span className="text-xs text-normal select-none hidden md:inline">
            {t('loopAutomation.label')}
          </span>
          <Switch
            checked={enabled}
            onCheckedChange={handleToggle}
            disabled={updatePolicy.isPending}
            aria-label={t('loopAutomation.label')}
          />
        </span>
      </Tooltip>

      {enabled && (
        <Popover open={open} onOpenChange={setOpen}>
          <PopoverTrigger asChild>
            <button
              type="button"
              aria-label={t('loopAutomation.settingsLabel')}
              title={t('loopAutomation.settingsLabel')}
              className="flex items-center justify-center p-half rounded-sm text-low hover:text-normal hover:bg-secondary/50 transition-colors focus:outline-none focus-visible:ring-1 focus-visible:ring-brand shrink-0"
            >
              <GearSixIcon className="size-icon-sm" weight="bold" aria-hidden />
            </button>
          </PopoverTrigger>
          <PopoverContent align="end" className="w-72">
            <div className="flex flex-col gap-base">
              <div className="flex flex-col gap-0.5">
                <h4 className="text-sm font-medium text-normal">
                  {t('loopAutomation.settingsTitle')}
                </h4>
                <p className="text-xs text-low">
                  {t('loopAutomation.settingsDescription')}
                </p>
              </div>

              <label className="flex flex-col gap-1.5">
                <span className="text-xs text-low">
                  {t('loopAutomation.retryInterval')}
                </span>
                <div className="flex items-center gap-2">
                  <input
                    type="number"
                    min={1}
                    inputMode="numeric"
                    value={intervalMinutes}
                    onChange={(e) => setIntervalMinutes(e.target.value)}
                    className={`${fieldClassName} w-20`}
                  />
                  <span className="text-xs text-low">
                    {t('loopAutomation.minutes')}
                  </span>
                </div>
              </label>

              <label className="flex flex-col gap-1.5">
                <span className="text-xs text-low">
                  {t('loopAutomation.maxAttempts')}
                </span>
                <input
                  type="number"
                  min={0}
                  inputMode="numeric"
                  value={maxAttempts}
                  onChange={(e) => setMaxAttempts(e.target.value)}
                  className={`${fieldClassName} w-20`}
                />
              </label>

              <label className="flex flex-col gap-1.5">
                <span className="text-xs text-low">
                  {t('loopAutomation.continuationPrompt')}
                </span>
                <textarea
                  rows={3}
                  value={continuationPrompt}
                  onChange={(e) => setContinuationPrompt(e.target.value)}
                  placeholder={t(
                    'loopAutomation.continuationPromptPlaceholder'
                  )}
                  className={`${fieldClassName} resize-none`}
                />
              </label>

              <div className="flex justify-end">
                <Button
                  size="xs"
                  onClick={handleSave}
                  disabled={updatePolicy.isPending}
                  className="bg-brand text-on-brand border-transparent hover:bg-brand-hover focus-visible:ring-brand"
                >
                  {updatePolicy.isPending
                    ? t('loopAutomation.saving')
                    : t('loopAutomation.save')}
                </Button>
              </div>
            </div>
          </PopoverContent>
        </Popover>
      )}
    </div>
  );
}
