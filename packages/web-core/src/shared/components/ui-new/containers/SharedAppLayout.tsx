import { useEffect } from 'react';
import { Outlet } from '@tanstack/react-router';
import { SyncErrorProvider } from '@/shared/providers/SyncErrorProvider';
import { useIsMobile } from '@/shared/hooks/useIsMobile';
import { useVisualViewportHeight } from '@/shared/hooks/useVisualViewportHeight';
import { useUiPreferencesStore } from '@/shared/stores/useUiPreferencesStore';
import { cn } from '@/shared/lib/utils';

import { NavbarContainer } from './NavbarContainer';
import { CommandBarDialog } from '@/shared/dialogs/command-bar/CommandBarDialog';
import { useCommandBarShortcut } from '@/shared/hooks/useCommandBarShortcut';
import { useCurrentAppDestination } from '@/shared/hooks/useCurrentAppDestination';
import { isLocalWorkspacesDestination } from '@/shared/lib/routes/appNavigation';
import { useWorkspaceSidebarPreviewController } from '@/shared/hooks/useWorkspaceSidebarPreviewController';
import { WorkspacesSidebarContainer } from '@/pages/workspaces/WorkspacesSidebarContainer';
import { WorkspacesSidebarReopenTag } from '@vibe/ui/components/WorkspacesSidebar';

export function SharedAppLayout() {
  const currentDestination = useCurrentAppDestination();
  const isMobile = useIsMobile();
  const visualViewportHeight = useVisualViewportHeight();
  const mobileFontScale = useUiPreferencesStore((s) => s.mobileFontScale);
  const isLeftSidebarVisible = useUiPreferencesStore(
    (s) => s.isLeftSidebarVisible
  );
  // Register CMD+K shortcut globally for all routes under SharedAppLayout
  useCommandBarShortcut(() => CommandBarDialog.show());

  // Apply mobile font scale CSS variable
  useEffect(() => {
    if (!isMobile) {
      document.documentElement.style.removeProperty('--mobile-font-scale');
      return;
    }
    const scaleMap = { default: '1', small: '0.9', smaller: '0.8' } as const;
    document.documentElement.style.setProperty(
      '--mobile-font-scale',
      scaleMap[mobileFontScale]
    );
    return () => {
      document.documentElement.style.removeProperty('--mobile-font-scale');
    };
  }, [isMobile, mobileFontScale]);

  const isWorkspacesActive = isLocalWorkspacesDestination(currentDestination);
  const isWorkspaceSidebarPreviewEnabled =
    !isMobile && isWorkspacesActive && !isLeftSidebarVisible;
  const sidebarPreview = useWorkspaceSidebarPreviewController({
    enabled: isWorkspaceSidebarPreviewEnabled,
    isAppBarHovered: false,
  });

  return (
    <SyncErrorProvider>
      <div
        className={cn(
          'bg-primary',
          isMobile
            ? 'flex fixed inset-x-0 top-0 pb-[env(safe-area-inset-bottom)]'
            : 'grid grid-rows-[auto_1fr] h-screen'
        )}
        style={
          isMobile
            ? {
                // Track the VISUAL viewport so the app (and the terminal's
                // input line) ends above the on-screen keyboard instead of
                // underneath it — iOS never shrinks the layout viewport (see
                // useVisualViewportHeight). Falls back to the layout viewport
                // when visualViewport is unavailable.
                height: visualViewportHeight ?? '100dvh',
              }
            : undefined
        }
      >
        {!isMobile && (
          <>
            {/* Desktop navbar. */}
            <NavbarContainer />
            {/* Desktop content. */}
            <div className="relative min-h-0 overflow-hidden">
              {isWorkspaceSidebarPreviewEnabled && (
                <div className="absolute inset-y-0 left-0 z-20 flex items-center">
                  <WorkspacesSidebarReopenTag
                    active={sidebarPreview.isPreviewOpen}
                    onHoverStart={sidebarPreview.handleHandleHoverStart}
                    onHoverEnd={sidebarPreview.handleHandleHoverEnd}
                    ariaLabel="Workspaces"
                  />
                </div>
              )}

              {isWorkspaceSidebarPreviewEnabled && (
                <div
                  className={cn(
                    'absolute left-0 top-0 z-30 h-full w-[300px] transition-transform duration-150 ease-out',
                    sidebarPreview.isPreviewOpen
                      ? 'translate-x-0 pointer-events-auto'
                      : '-translate-x-full pointer-events-none'
                  )}
                  onMouseEnter={sidebarPreview.handlePreviewHoverStart}
                  onMouseLeave={sidebarPreview.handlePreviewHoverEnd}
                >
                  <div className="h-full w-full overflow-hidden border-r border-border bg-secondary shadow-lg">
                    <WorkspacesSidebarContainer />
                  </div>
                </div>
              )}

              <Outlet />
            </div>
          </>
        )}

        {isMobile && (
          <div className="flex flex-col flex-1 min-w-0 overflow-hidden">
            <NavbarContainer mobileMode={isMobile} />
            <div className="flex-1 min-h-0 overflow-hidden">
              <Outlet />
            </div>
          </div>
        )}
      </div>
    </SyncErrorProvider>
  );
}
