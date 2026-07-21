import { useEffect, useMemo, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { useWorkspaceContext } from '@/shared/hooks/useWorkspaceContext';
import { useActions } from '@/shared/hooks/useActions';
import { useSyncErrorContext } from '@/shared/hooks/useSyncErrorContext';
import { useUserOrganizations } from '@/shared/hooks/useUserOrganizations';
import { useOrganizationStore } from '@/shared/stores/useOrganizationStore';
import {
  MOBILE_TABS,
  Navbar,
  type MobileTabId,
  type NavbarSectionItem,
} from '@vibe/ui/components/Navbar';
import { Tooltip } from '@vibe/ui/components/Tooltip';
import { AppBarUserPopoverContainer } from './AppBarUserPopoverContainer';
import { AppBarNotificationBellContainer } from '@/pages/workspaces/AppBarNotificationBellContainer';
import { useUserSystem } from '@/shared/hooks/useUserSystem';
import { useAppUpdateStore } from '@/shared/stores/useAppUpdateStore';
import { useAuth } from '@/shared/hooks/auth/useAuth';
import { isTauriMac } from '@/shared/lib/platform';
import { Actions, NavbarActionGroups } from '@/shared/actions';
import { TerminalWindowIcon } from '@phosphor-icons/react';
import {
  NavbarDivider,
  type ActionDefinition,
  type NavbarItem as ActionNavbarItem,
  type ActionVisibilityContext,
  isSpecialIcon,
  getActionIcon,
  getActionTooltip,
  isActionActive,
  isActionEnabled,
  isActionVisible,
} from '@/shared/types/actions';
import { useActionVisibilityContext } from '@/shared/hooks/useActionVisibilityContext';
import { useMobileActiveTab } from '@/shared/stores/useUiPreferencesStore';
import { CommandBarDialog } from '@/shared/dialogs/command-bar/CommandBarDialog';
import { SettingsDialog } from '@/shared/dialogs/settings/SettingsDialog';
import { useAppNavigation } from '@/shared/hooks/useAppNavigation';
import { getRemoteAuthDegradedMessage } from '@/shared/lib/auth/remoteAuthDegraded';
import { useHostId } from '@/shared/providers/HostIdProvider';

/**
 * Check if a NavbarItem is a divider
 */
function isDivider(item: ActionNavbarItem): item is typeof NavbarDivider {
  return 'type' in item && item.type === 'divider';
}

/**
 * Filter navbar items by visibility, keeping dividers but removing them
 * if they would appear at the start, end, or consecutively.
 */
function filterNavbarItems(
  items: readonly ActionNavbarItem[],
  ctx: ActionVisibilityContext
): ActionNavbarItem[] {
  // Filter actions by visibility, keep dividers
  const filtered = items.filter((item) => {
    if (isDivider(item)) return true;
    if (!isActionVisible(item, ctx)) return false;
    return !isSpecialIcon(getActionIcon(item, ctx));
  });

  // Remove leading/trailing dividers and consecutive dividers
  const result: ActionNavbarItem[] = [];
  for (const item of filtered) {
    if (isDivider(item)) {
      // Only add divider if we have items before it and last item wasn't a divider
      if (result.length > 0 && !isDivider(result[result.length - 1])) {
        result.push(item);
      }
    } else {
      result.push(item);
    }
  }

  // Remove trailing divider
  if (result.length > 0 && isDivider(result[result.length - 1])) {
    result.pop();
  }

  return result;
}

function toNavbarSectionItems(
  items: readonly ActionNavbarItem[],
  ctx: ActionVisibilityContext,
  onExecuteAction: (action: ActionDefinition) => void
): NavbarSectionItem[] {
  return items.reduce<NavbarSectionItem[]>((result, item) => {
    if (isDivider(item)) {
      result.push({ type: 'divider' });
      return result;
    }

    const icon = getActionIcon(item, ctx);
    if (isSpecialIcon(icon)) {
      return result;
    }

    result.push({
      type: 'action',
      id: item.id,
      icon,
      isActive: isActionActive(item, ctx),
      tooltip: getActionTooltip(item, ctx),
      shortcut: item.shortcut,
      disabled: !isActionEnabled(item, ctx),
      onClick: () => onExecuteAction(item),
    });
    return result;
  }, []);
}

export function NavbarContainer({
  mobileMode = false,
}: {
  mobileMode?: boolean;
}) {
  const { t } = useTranslation('common');
  const hostId = useHostId();
  const { executeAction } = useActions();
  const { workspace: selectedWorkspace, isCreateMode } = useWorkspaceContext();
  const syncErrorContext = useSyncErrorContext();
  const { remoteAuthDegraded, appVersion } = useUserSystem();
  const updateVersion = useAppUpdateStore((s) => s.updateVersion);
  const restartForUpdate = useAppUpdateStore((s) => s.restart);
  const { isSignedIn } = useAuth();
  const appNavigation = useAppNavigation();
  const [mobileActiveTab, setMobileActiveTab] = useMobileActiveTab();
  const mobileTabs = useMemo(
    () => (hostId ? MOBILE_TABS.filter((t) => t.id !== 'files') : MOBILE_TABS),
    [hostId]
  );

  const { data: orgsData } = useUserOrganizations();
  const organizations = useMemo(
    () => orgsData?.organizations ?? [],
    [orgsData?.organizations]
  );
  const selectedOrgId = useOrganizationStore((s) => s.selectedOrgId);
  const setSelectedOrgId = useOrganizationStore((s) => s.setSelectedOrgId);

  // Auto-select first org if none selected or selection is invalid
  useEffect(() => {
    if (organizations.length === 0) return;

    const hasValidSelection = selectedOrgId
      ? organizations.some((org) => org.id === selectedOrgId)
      : false;

    if (!selectedOrgId || !hasValidSelection) {
      const firstNonPersonal = organizations.find((org) => !org.is_personal);
      setSelectedOrgId((firstNonPersonal ?? organizations[0]).id);
    }
  }, [organizations, selectedOrgId, setSelectedOrgId]);

  // Get action visibility context (includes all state for visibility/active/enabled)
  const actionCtx = useActionVisibilityContext();

  // Action handler - all actions go through the standard executeAction
  const handleExecuteAction = useCallback(
    (action: ActionDefinition) => {
      if (action.requiresTarget && selectedWorkspace?.id) {
        executeAction(action, selectedWorkspace.id);
      } else {
        executeAction(action);
      }
    },
    [executeAction, selectedWorkspace?.id]
  );

  const leftItems = useMemo(
    () =>
      toNavbarSectionItems(
        filterNavbarItems(NavbarActionGroups.left, actionCtx),
        actionCtx,
        handleExecuteAction
      ),
    [actionCtx, handleExecuteAction]
  );

  const rightItems = useMemo(
    () =>
      toNavbarSectionItems(
        filterNavbarItems(NavbarActionGroups.right, actionCtx),
        actionCtx,
        handleExecuteAction
      ),
    [actionCtx, handleExecuteAction]
  );

  const navbarTitle = isCreateMode
    ? 'Create Workspace'
    : selectedWorkspace?.branch;

  // Mobile-specific callbacks
  const handleOpenCommandBar = useCallback(() => {
    CommandBarDialog.show();
  }, []);

  const handleOpenSettings = useCallback(() => {
    SettingsDialog.show();
  }, []);

  const handleNavigateBack = useCallback(() => {
    appNavigation.goToWorkspaces();
  }, [appNavigation]);

  const userPopover = useMemo(
    () => (
      <AppBarUserPopoverContainer
        organizations={organizations}
        selectedOrgId={selectedOrgId ?? ''}
        onOrgSelect={setSelectedOrgId}
      />
    ),
    [organizations, selectedOrgId, setSelectedOrgId]
  );

  const brand = useMemo(
    () => (
      <button
        type="button"
        onClick={() => appNavigation.goToWorkspaces()}
        className="font-ibm-plex-mono text-sm font-semibold tracking-tight text-high hover:opacity-80 transition-opacity cursor-pointer select-none whitespace-nowrap"
        aria-label="BetterCoding — home"
      >
        Better<span className="text-brand">Coding</span>
      </button>
    ),
    [appNavigation]
  );

  // App version (or pending update) — left-most element of the right section.
  const versionSlot = useMemo(() => {
    if (updateVersion) {
      return (
        <Tooltip content={`Update to v${updateVersion}`}>
          <button
            type="button"
            onClick={restartForUpdate ?? undefined}
            className="px-1.5 py-0.5 rounded-sm text-[10px] font-ibm-plex-mono font-medium leading-none bg-brand text-on-brand hover:bg-brand-hover transition-colors cursor-pointer"
          >
            Update
          </button>
        </Tooltip>
      );
    }
    if (!appVersion) {
      return undefined;
    }
    return (
      <span
        className="text-[10px] font-ibm-plex-mono text-low leading-none select-none"
        title={`v${appVersion}`}
      >
        v{appVersion}
      </span>
    );
  }, [appVersion, updateVersion, restartForUpdate]);

  // Mobile keeps the action rail hidden, so CLI mode gets a dedicated
  // always-visible toggle in the top-right cluster — without it phones have
  // no way to switch between chat and the terminal.
  const mobileActionsSlot = useMemo(() => {
    if (!mobileMode || !isActionVisible(Actions.ToggleCliMode, actionCtx)) {
      return undefined;
    }
    const cliActive = isActionActive(Actions.ToggleCliMode, actionCtx);
    return (
      <button
        type="button"
        className={
          cliActive
            ? 'flex items-center justify-center text-brand'
            : 'flex items-center justify-center text-low hover:text-normal'
        }
        onClick={() => handleExecuteAction(Actions.ToggleCliMode)}
        aria-label={getActionTooltip(Actions.ToggleCliMode, actionCtx)}
      >
        <TerminalWindowIcon
          className="size-icon-sm"
          weight={cliActive ? 'fill' : 'regular'}
        />
      </button>
    );
  }, [mobileMode, actionCtx, handleExecuteAction]);

  const rightEnd = useMemo(() => {
    if (mobileMode) {
      return undefined;
    }
    return (
      <div className="flex items-center gap-base">
        <div className="h-4 w-px bg-border" />
        {isSignedIn && <AppBarNotificationBellContainer />}
        {userPopover}
      </div>
    );
  }, [mobileMode, isSignedIn, userPopover]);

  const syncErrors = useMemo(() => {
    const errors = syncErrorContext?.errors ? [...syncErrorContext.errors] : [];

    if (remoteAuthDegraded) {
      errors.push({
        streamId: 'remote-auth-degraded',
        tableName: 'Remote authentication',
        error: {
          message: getRemoteAuthDegradedMessage(remoteAuthDegraded, t),
        },
        retry: () => window.location.reload(),
      });
    }

    return errors;
  }, [remoteAuthDegraded, syncErrorContext?.errors, t]);

  return (
    <Navbar
      workspaceTitle={navbarTitle}
      brand={mobileMode ? undefined : brand}
      leftItems={leftItems}
      rightItems={rightItems}
      rightStart={versionSlot}
      rightEnd={rightEnd}
      syncErrors={syncErrors}
      className={!mobileMode && isTauriMac() ? 'pl-16' : undefined}
      mobileMode={mobileMode}
      mobileUserSlot={mobileMode ? userPopover : undefined}
      mobileActionsSlot={mobileActionsSlot}
      onOpenCommandBar={handleOpenCommandBar}
      onOpenSettings={handleOpenSettings}
      onNavigateBack={handleNavigateBack}
      mobileActiveTab={mobileActiveTab}
      onMobileTabChange={(tab: MobileTabId) => setMobileActiveTab(tab)}
      mobileTabs={mobileTabs}
    />
  );
}
