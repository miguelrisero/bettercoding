import { useRef } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { createRoot } from 'react-dom/client';
import {
  WorkspaceInputsContainer,
  WorkspaceInputViewer,
} from '@/pages/workspaces/WorkspaceInputsContainer';
import { useIsMobile } from '@/shared/hooks/useIsMobile';
import { useMobileActiveTab } from '@/shared/stores/useUiPreferencesStore';
import '@/app/styles/new/index.css';

// Use the same tab visibility as the workspace shell without starting it.
function QueueFixture() {
  const isMobile = useIsMobile();
  const [tab] = useMobileActiveTab();
  const list = useRef<HTMLElement>(null);
  const workspaceId = '00000000-0000-4000-8000-000000000099';
  return (
    <>
      <div hidden={isMobile && tab !== 'inputs'}>
        <WorkspaceInputsContainer workspaceId={workspaceId} listRef={list} />
      </div>
      <WorkspaceInputViewer
        workspaceId={workspaceId}
        mobileListRef={isMobile ? list : undefined}
      />
    </>
  );
}

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false } },
});
createRoot(document.getElementById('root')!).render(
  <QueryClientProvider client={queryClient}>
    <QueueFixture />
  </QueryClientProvider>
);
