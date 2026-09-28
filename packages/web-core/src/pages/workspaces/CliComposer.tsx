import { useSyncExternalStore } from 'react';
import { useTranslation } from 'react-i18next';
import { CircleNotchIcon, PaperPlaneRightIcon } from '@phosphor-icons/react';

import { cliAgentApi } from '@/shared/lib/api';
import {
  CLI_SEND_MAX_BYTES,
  createComposerStore,
  isComposerSendKey,
  type ComposerNotice,
} from '@/shared/lib/cliComposer';

interface CliComposerProps {
  workspaceId: string;
}

// Drafts, in-flight sends and outcomes per workspace, kept for the page's
// lifetime so hiding the composer or switching workspaces keeps them.
const composerStore = createComposerStore();

/**
 * Opt-in text box under the CLI terminal: the text is written locally (no
 * per-keystroke round trip through tmux) and sent whole to the pane's agent
 * on Enter.
 */
export function CliComposer({ workspaceId }: CliComposerProps) {
  const { t } = useTranslation('common');
  const { text, sending, notice } = useSyncExternalStore(
    composerStore.subscribe,
    () => composerStore.get(workspaceId)
  );
  const canSend = !sending && text.trim() !== '';
  const submit = () =>
    composerStore.submit(workspaceId, (value) =>
      cliAgentApi.sendText(workspaceId, { text: value })
    );

  const noticeText = (n: ComposerNotice) => {
    switch (n.kind) {
      case 'unsubmitted':
        return t('cliMode.composer.unsubmitted');
      case 'noAgent':
        return t('cliMode.composer.noAgent');
      case 'notDelivered':
        return t('cliMode.composer.notDelivered');
      case 'empty':
        return t('cliMode.composer.empty');
      case 'controlChars':
        return t('cliMode.composer.controlChars');
      case 'tooLarge':
        return t('cliMode.composer.tooLarge', {
          size: Math.ceil(n.bytes / 1024),
          limit: CLI_SEND_MAX_BYTES / 1024,
        });
      case 'failed':
        return t('cliMode.composer.failed', { message: n.message });
    }
  };

  // The layout reserves one row (h-12) and the box grows upward over the
  // terminal's bottom rows. Growing the reserved space instead would refit
  // the terminal and send a tmux resize (and a full TUI redraw) for every
  // line the draft gains.
  return (
    <div className="relative h-12 shrink-0">
      <div className="absolute inset-x-0 bottom-0 z-10 flex flex-col gap-1">
        <p
          aria-live="polite"
          className={
            notice
              ? `self-start rounded-md bg-secondary px-2 py-1 text-xs ${notice.kind === 'unsubmitted' ? 'text-low' : 'text-error'}`
              : 'sr-only'
          }
        >
          {notice ? noticeText(notice) : ''}
        </p>
        <div className="flex items-end gap-2 rounded-md border border-border bg-secondary px-2 py-1">
          <textarea
            value={text}
            onChange={(e) => composerStore.setText(workspaceId, e.target.value)}
            onKeyDown={(e) => {
              if (
                isComposerSendKey({
                  key: e.key,
                  shiftKey: e.shiftKey,
                  isComposing: e.nativeEvent.isComposing,
                  keyCode: e.keyCode,
                })
              ) {
                e.preventDefault();
                void submit();
              }
            }}
            readOnly={sending}
            rows={2}
            enterKeyHint="send"
            aria-label={t('cliMode.composer.label')}
            placeholder={t('cliMode.composer.placeholder')}
            // field-sizing grows the box with its content (bounded by
            // max-h); browsers without it keep the two-row box and scroll.
            className="min-w-0 flex-1 resize-none bg-transparent text-sm text-normal placeholder:text-low focus:outline-none [field-sizing:content] min-h-[1.25rem] max-h-40"
          />
          <button
            type="button"
            onClick={() => void submit()}
            // Keep focus (and the soft keyboard) on the text box.
            onPointerDown={(e) => e.preventDefault()}
            disabled={!canSend}
            aria-busy={sending}
            aria-label={t('cliMode.composer.send')}
            title={t('cliMode.composer.send')}
            className="flex items-center justify-center size-8 shrink-0 rounded-md text-low hover:text-normal hover:bg-primary transition-colors disabled:opacity-50 disabled:hover:bg-transparent disabled:hover:text-low"
          >
            {sending ? (
              <CircleNotchIcon
                className="size-icon-sm animate-spin motion-reduce:animate-none"
                weight="bold"
                aria-hidden="true"
              />
            ) : (
              <PaperPlaneRightIcon className="size-icon-sm" weight="bold" />
            )}
          </button>
        </div>
      </div>
    </div>
  );
}
