import { useContext } from 'react';
import { createHmrContext } from '@/shared/lib/hmrContext';

/**
 * False inside a pane that stays mounted while hidden (the chat pane under
 * the shown CLI pane). Pane-local polling and keyboard scopes pause while it
 * is false; the pane resumes from its cached state when shown again.
 */
export const PaneVisibilityContext = createHmrContext<boolean>(
  'PaneVisibilityContext',
  true
);

export function usePaneVisible(): boolean {
  return useContext(PaneVisibilityContext);
}
