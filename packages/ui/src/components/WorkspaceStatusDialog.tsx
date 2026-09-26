import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from './KeyboardDialog';
import { Button } from './Button';
import { Input } from './Input';
import NiceModal, { useModal } from '@ebay/nice-modal-react';
import { defineModal } from '../lib/modals';

export type WorkspaceStatusKind = 'locked' | 'seen' | null;

export interface WorkspaceStatusDialogProps {
  currentNote?: string | null;
  onSubmit: (kind: WorkspaceStatusKind, note: string | null) => Promise<void>;
}

const WorkspaceStatusDialogImpl = NiceModal.create<WorkspaceStatusDialogProps>(
  ({ currentNote, onSubmit }) => {
    const modal = useModal();
    const { t } = useTranslation(['common']);
    const [note, setNote] = useState(currentNote ?? '');
    const [error, setError] = useState<string | null>(null);
    const [isSubmitting, setIsSubmitting] = useState(false);

    const submit = async (kind: WorkspaceStatusKind) => {
      setIsSubmitting(true);
      setError(null);
      try {
        await onSubmit(kind, kind === 'locked' ? note.trim() || null : null);
        modal.resolve();
        modal.hide();
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setIsSubmitting(false);
      }
    };

    return (
      <Dialog
        open={modal.visible}
        onOpenChange={(open) => {
          if (!open) {
            modal.resolve();
            modal.hide();
          }
        }}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>
              {t('workspaces.status.title', { defaultValue: 'Set status' })}
            </DialogTitle>
            <DialogDescription>
              {t('workspaces.status.description', {
                defaultValue:
                  'Lock parks the workspace with an optional note. Mark seen dismisses a finished turn. Either one clears as soon as the agent works again.',
              })}
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-2">
            <label htmlFor="workspace-status-note" className="text-sm">
              {t('workspaces.status.noteLabel', {
                defaultValue: 'Note (for Lock)',
              })}
            </label>
            <Input
              id="workspace-status-note"
              type="text"
              value={note}
              maxLength={48}
              onChange={(e) => setNote(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !isSubmitting) void submit('locked');
              }}
              placeholder={t('workspaces.status.notePlaceholder', {
                defaultValue: 'waiting on …',
              })}
              disabled={isSubmitting}
              autoFocus
            />
            {error && <p className="text-sm text-destructive">{error}</p>}
          </div>

          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => void submit(null)}
              disabled={isSubmitting}
            >
              {t('workspaces.status.clear', { defaultValue: 'Clear' })}
            </Button>
            <Button
              variant="outline"
              onClick={() => void submit('seen')}
              disabled={isSubmitting}
            >
              {t('workspaces.status.seen', { defaultValue: 'Mark seen' })}
            </Button>
            <Button
              onClick={() => void submit('locked')}
              disabled={isSubmitting}
            >
              {t('workspaces.status.lock', { defaultValue: 'Lock' })}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  }
);

export const WorkspaceStatusDialog = defineModal<
  WorkspaceStatusDialogProps,
  void
>(WorkspaceStatusDialogImpl);
