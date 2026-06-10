import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { useLocation, useNavigate, useParams } from "@tanstack/react-router";
import { XIcon, PlusIcon, HouseIcon, LinkIcon } from "@phosphor-icons/react";
import { MobileDrawer } from "@vibe/ui/components/MobileDrawer";
import { Tooltip } from "@vibe/ui/components/Tooltip";
import { useIsMobile } from "@/shared/hooks/useIsMobile";
import { cn } from "@/shared/lib/utils";
import { useUserOrganizations } from "@/shared/hooks/useUserOrganizations";
import { useAuth } from "@/shared/hooks/auth/useAuth";
import { useOrganizationStore } from "@/shared/stores/useOrganizationStore";
import { AppBarNotificationBellContainer } from "@/pages/workspaces/AppBarNotificationBellContainer";
import { SettingsDialog } from "@/shared/dialogs/settings/SettingsDialog";
import { CommandBarDialog } from "@/shared/dialogs/command-bar/CommandBarDialog";
import { useCommandBarShortcut } from "@/shared/hooks/useCommandBarShortcut";
import { RemoteAppBarUserPopoverContainer } from "@remote/app/layout/RemoteAppBarUserPopoverContainer";
import { RemoteNavbarContainer } from "@remote/app/layout/RemoteNavbarContainer";
import { RemoteDesktopNavbar } from "@remote/app/layout/RemoteDesktopNavbar";
import {
  useRelayAppBarHosts,
  type RelayAppBarHostStatus,
} from "@remote/shared/hooks/useRelayAppBarHosts";

interface RemoteAppShellProps {
  children: ReactNode;
}

function getHostInitials(name: string): string {
  const trimmed = name.trim();
  if (!trimmed) return "??";
  const words = trimmed.split(/\s+/);
  if (words.length >= 2) {
    return (words[0][0] + words[1][0]).toUpperCase();
  }
  return trimmed.slice(0, 2).toUpperCase();
}

function getHostStatusLabel(status: RelayAppBarHostStatus): string {
  if (status === "online") return "Online";
  if (status === "offline") return "Offline";
  return "Unpaired";
}

export function RemoteAppShell({ children }: RemoteAppShellProps) {
  const navigate = useNavigate();
  const location = useLocation();
  const { hostId: routeHostId } = useParams({ strict: false });
  const { isSignedIn } = useAuth();
  const isWorkspaceContextRoute = location.pathname.includes("/workspaces");

  useCommandBarShortcut(() => CommandBarDialog.show(), isWorkspaceContextRoute);
  const isMobile = useIsMobile();
  const [isDrawerOpen, setIsDrawerOpen] = useState(false);

  const { data: organizationsData } = useUserOrganizations();
  const organizations = organizationsData?.organizations ?? [];
  const selectedOrgId = useOrganizationStore((s) => s.selectedOrgId);
  const setSelectedOrgId = useOrganizationStore((s) => s.setSelectedOrgId);

  useEffect(() => {
    if (organizations.length === 0) {
      return;
    }

    const hasValidSelection = selectedOrgId
      ? organizations.some((organization) => organization.id === selectedOrgId)
      : false;

    if (!hasValidSelection) {
      const firstOrg = organizations.find(
        (organization) => !organization.is_personal,
      );
      setSelectedOrgId((firstOrg ?? organizations[0]).id);
    }
  }, [organizations, selectedOrgId, setSelectedOrgId]);

  const { hosts: relayHosts } = useRelayAppBarHosts(isSignedIn);

  const selectedOrgName =
    organizations.find((organization) => organization.id === selectedOrgId)
      ?.name ?? null;

  const activeHostId = routeHostId ?? null;

  const openRelaySettings = useCallback((hostId?: string) => {
    void SettingsDialog.show({
      initialSection: "relay",
      ...(hostId ? { initialState: { hostId } } : {}),
    });
  }, []);

  const handleHostClick = useCallback(
    (hostId: string, status: RelayAppBarHostStatus) => {
      if (status === "online") {
        navigate({
          to: "/hosts/$hostId/workspaces",
          params: { hostId },
        });
        return;
      }

      if (status !== "unpaired") {
        return;
      }

      openRelaySettings(hostId);
    },
    [navigate, openRelaySettings],
  );

  const handlePairHostClick = useCallback(() => {
    openRelaySettings();
  }, [openRelaySettings]);

  const userPopover = (
    <RemoteAppBarUserPopoverContainer
      organizations={organizations}
      selectedOrgId={selectedOrgId ?? ""}
      onOrgSelect={setSelectedOrgId}
    />
  );

  const mobileUserSlot = useMemo(() => {
    if (!isMobile) return undefined;
    return (
      <RemoteAppBarUserPopoverContainer
        organizations={organizations}
        selectedOrgId={selectedOrgId ?? ""}
        onOrgSelect={setSelectedOrgId}
      />
    );
  }, [isMobile, organizations, selectedOrgId, setSelectedOrgId]);

  return (
    <div
      className={cn(
        "flex flex-col bg-primary",
        isMobile
          ? "fixed inset-0 pb-[env(safe-area-inset-bottom)]"
          : "h-screen",
      )}
    >
      <div className="flex min-h-0 flex-1">
        {!isMobile && (
          <div
            className={cn(
              "flex flex-col items-center h-full min-h-0 overflow-y-auto p-base gap-base",
              "bg-secondary border-r border-border",
            )}
          >
            {/* Home */}
            <Tooltip content="Home" side="right">
              <button
                type="button"
                onClick={() => navigate({ to: "/" })}
                className="flex items-center justify-center w-10 h-10 rounded-lg bg-primary text-normal hover:bg-brand/10 cursor-pointer"
                aria-label="Home"
              >
                <HouseIcon className="size-icon-base" weight="bold" />
              </button>
            </Tooltip>

            {/* Hosts */}
            {isSignedIn &&
              relayHosts.map((host) => {
                const isOffline = host.status === "offline";
                return (
                  <Tooltip
                    key={host.id}
                    content={`${host.name} · ${getHostStatusLabel(host.status)}`}
                    side="right"
                  >
                    <div className="relative">
                      <span
                        className={cn(
                          "absolute -top-1 -right-1 z-10",
                          "w-3.5 h-3.5 rounded-full border border-secondary",
                          host.status === "online"
                            ? "bg-success"
                            : host.status === "offline"
                              ? "bg-low"
                              : "bg-white border-warning",
                        )}
                        aria-hidden="true"
                      />
                      <button
                        type="button"
                        disabled={isOffline}
                        onClick={() => handleHostClick(host.id, host.status)}
                        className={cn(
                          "flex items-center justify-center w-10 h-10 rounded-lg text-sm font-medium",
                          isOffline
                            ? "bg-primary text-low opacity-50 cursor-not-allowed"
                            : host.id === activeHostId
                              ? "bg-brand/20 text-brand cursor-pointer"
                              : "bg-primary text-normal cursor-pointer hover:bg-brand/10",
                        )}
                        aria-label={`${host.name} (${getHostStatusLabel(host.status)})`}
                      >
                        {getHostInitials(host.name)}
                      </button>
                    </div>
                  </Tooltip>
                );
              })}

            {/* Pair a host */}
            {isSignedIn && (
              <Tooltip content="Pair a remote device" side="right">
                <button
                  type="button"
                  onClick={handlePairHostClick}
                  className="flex items-center justify-center w-10 h-10 rounded-lg bg-primary text-muted hover:text-normal hover:bg-tertiary cursor-pointer"
                  aria-label="Pair a remote device"
                >
                  <LinkIcon className="size-icon-base" weight="bold" />
                </button>
              </Tooltip>
            )}

            {/* Bottom: notifications + user */}
            <div className="mt-auto pt-base flex flex-col items-center gap-4">
              {isSignedIn && <AppBarNotificationBellContainer />}
              {userPopover}
            </div>
          </div>
        )}

        <MobileDrawer
          open={isDrawerOpen && isMobile}
          onClose={() => setIsDrawerOpen(false)}
        >
          <div className="flex flex-col h-full">
            {/* Header: org name + close button */}
            <div className="flex items-center justify-between p-4 border-b border-border">
              <span className="text-sm font-medium text-high truncate">
                {selectedOrgName ?? "Organization"}
              </span>
              <button
                type="button"
                onClick={() => setIsDrawerOpen(false)}
                className="p-1 rounded-sm text-low hover:text-normal cursor-pointer"
              >
                <XIcon className="h-4 w-4" weight="bold" />
              </button>
            </div>

            {/* Home link */}
            <button
              type="button"
              onClick={() => {
                navigate({ to: "/" });
                setIsDrawerOpen(false);
              }}
              className="flex items-center gap-2 px-4 py-3 text-sm text-normal hover:bg-secondary cursor-pointer"
            >
              <HouseIcon className="h-4 w-4" />
              Home
            </button>

            {/* Divider */}
            <div className="mx-3 border-t border-border" />

            {/* Hosts section */}
            {isSignedIn && relayHosts.length > 0 && (
              <>
                <p className="px-4 pt-3 pb-1 text-xs font-medium uppercase tracking-wide text-low">
                  Hosts
                </p>
                <div className="px-2">
                  {relayHosts.map((host) => {
                    const isOnline = host.status === "online";
                    const isUnpaired = host.status === "unpaired";
                    const isClickable = isOnline || isUnpaired;

                    return (
                      <button
                        key={host.id}
                        type="button"
                        disabled={!isClickable}
                        onClick={() => {
                          handleHostClick(host.id, host.status);
                          setIsDrawerOpen(false);
                        }}
                        className={cn(
                          "flex items-center gap-3 w-full px-3 py-2 rounded-md text-sm text-left",
                          "transition-colors",
                          isClickable
                            ? "cursor-pointer hover:bg-secondary"
                            : "opacity-50",
                        )}
                      >
                        <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-brand/15 text-xs font-semibold text-brand">
                          {getHostInitials(host.name)}
                        </div>
                        <span className="min-w-0 flex-1 truncate text-normal">
                          {host.name}
                        </span>
                        <span
                          className={cn(
                            "h-2 w-2 shrink-0 rounded-full",
                            isOnline
                              ? "bg-success"
                              : isUnpaired
                                ? "border border-warning bg-white"
                                : "bg-low",
                          )}
                        />
                      </button>
                    );
                  })}
                </div>
              </>
            )}

            {/* Link a host button */}
            {isSignedIn && (
              <div className="px-2">
                <button
                  type="button"
                  onClick={() => {
                    handlePairHostClick();
                    setIsDrawerOpen(false);
                  }}
                  className="flex items-center gap-3 w-full px-3 py-2 rounded-md text-sm text-low hover:text-normal hover:bg-secondary cursor-pointer"
                >
                  <PlusIcon className="h-4 w-4" />
                  Link a host
                </button>
              </div>
            )}
          </div>
        </MobileDrawer>

        <div className="flex min-w-0 flex-1 flex-col">
          {isMobile && isWorkspaceContextRoute && (
            <RemoteNavbarContainer
              mobileMode={isMobile}
              mobileUserSlot={mobileUserSlot}
            />
          )}
          {!isMobile && isWorkspaceContextRoute && <RemoteDesktopNavbar />}
          <div className="min-h-0 flex-1">{children}</div>
        </div>
      </div>
    </div>
  );
}
