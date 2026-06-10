import { useCallback, useEffect, useMemo, type ReactNode } from "react";
import { useLocation, useNavigate, useParams } from "@tanstack/react-router";
import {
  MOBILE_TABS,
  Navbar,
  type MobileTabId,
} from "@vibe/ui/components/Navbar";
import { SettingsDialog } from "@/shared/dialogs/settings/SettingsDialog";
import { CommandBarDialog } from "@/shared/dialogs/command-bar/CommandBarDialog";
import { useMobileActiveTab } from "@/shared/stores/useUiPreferencesStore";
import { useMobileWorkspaceTitle } from "@remote/shared/stores/useMobileWorkspaceTitle";

interface RemoteNavbarContainerProps {
  mobileMode?: boolean;
  mobileUserSlot?: ReactNode;
}

export function RemoteNavbarContainer({
  mobileMode,
  mobileUserSlot,
}: RemoteNavbarContainerProps) {
  const location = useLocation();
  const { hostId } = useParams({ strict: false });
  const mobileWorkspaceTitle = useMobileWorkspaceTitle((s) => s.title);

  const [mobileActiveTab, setMobileActiveTab] = useMobileActiveTab();

  const remoteMobileTabs = useMemo(
    () =>
      MOBILE_TABS.filter((t) => t.id !== "preview" && t.id !== "workspaces"),
    [],
  );

  const isOnWorkspaceView = /^\/hosts\/[^/]+\/workspaces\/[^/]+/.test(
    location.pathname,
  );
  const isOnWorkspaceList = /^\/hosts\/[^/]+\/workspaces\/?$/.test(
    location.pathname,
  );

  useEffect(() => {
    if (isOnWorkspaceView) {
      setMobileActiveTab("chat");
    }
  }, [isOnWorkspaceView, setMobileActiveTab]);
  const navigate = useNavigate();

  const workspaceTitle = useMemo(() => {
    if (isOnWorkspaceView) {
      return mobileWorkspaceTitle ?? undefined;
    }
    return undefined;
  }, [isOnWorkspaceView, mobileWorkspaceTitle]);

  const mobileShowBack = isOnWorkspaceView || isOnWorkspaceList;

  const handleNavigateBack = useCallback(() => {
    if (isOnWorkspaceView) {
      if (!hostId) {
        navigate({ to: "/" });
        return;
      }
      navigate({ to: "/hosts/$hostId/workspaces", params: { hostId } });
    } else {
      navigate({ to: "/" });
    }
  }, [navigate, hostId, isOnWorkspaceView]);

  const handleOpenSettings = useCallback(() => {
    SettingsDialog.show();
  }, []);

  const handleOpenCommandBar = useCallback(() => {
    CommandBarDialog.show();
  }, []);

  return (
    <Navbar
      workspaceTitle={workspaceTitle}
      mobileMode={mobileMode}
      mobileUserSlot={mobileUserSlot}
      onNavigateBack={handleNavigateBack}
      mobileShowBack={mobileShowBack}
      onOpenSettings={handleOpenSettings}
      onOpenCommandBar={handleOpenCommandBar}
      mobileActiveTab={mobileActiveTab as MobileTabId}
      onMobileTabChange={(tab) => setMobileActiveTab(tab)}
      mobileTabs={remoteMobileTabs}
      showMobileTabs={isOnWorkspaceView}
    />
  );
}
