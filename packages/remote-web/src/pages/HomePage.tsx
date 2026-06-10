import { useCallback, useEffect, type ReactNode } from "react";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { clearTokens } from "@remote/shared/lib/auth";
import { SettingsDialog } from "@/shared/dialogs/settings/SettingsDialog";
import { useOrganizationStore } from "@/shared/stores/useOrganizationStore";
import { useUserOrganizations } from "@/shared/hooks/useUserOrganizations";
import { useAuth } from "@/shared/hooks/auth/useAuth";
import { useRelayAppBarHosts } from "@remote/shared/hooks/useRelayAppBarHosts";

function getHostInitials(name: string): string {
  const trimmed = name.trim();
  if (!trimmed) return "??";
  const words = trimmed.split(/\s+/);
  if (words.length >= 2) {
    return (words[0][0] + words[1][0]).toUpperCase();
  }
  return trimmed.slice(0, 2).toUpperCase();
}

export default function HomePage() {
  const navigate = useNavigate();
  const search = useSearch({ from: "/" });
  const setSelectedOrgId = useOrganizationStore((s) => s.setSelectedOrgId);
  const { isLoading: orgsLoading, error: orgsError } = useUserOrganizations();
  const { isSignedIn } = useAuth();
  const { hosts } = useRelayAppBarHosts(isSignedIn);

  const openRelaySettings = useCallback((hostId?: string) => {
    void SettingsDialog.show({
      initialSection: "relay",
      ...(hostId ? { initialState: { hostId } } : {}),
    });
  }, []);

  useEffect(() => {
    const legacyOrgId = search.legacyOrgSettingsOrgId;
    if (!legacyOrgId) {
      return;
    }

    setSelectedOrgId(legacyOrgId);
    navigate({
      to: "/",
      search: {},
      replace: true,
    });

    void SettingsDialog.show({
      initialSection: "organizations",
      initialState: { organizationId: legacyOrgId },
    });
  }, [navigate, search.legacyOrgSettingsOrgId, setSelectedOrgId]);

  const handleSignInAgain = async () => {
    await clearTokens();
    navigate({
      to: "/account",
      replace: true,
    });
  };

  const displayError = orgsError
    ? orgsError instanceof Error
      ? orgsError.message
      : "Failed to load organizations"
    : null;

  if (orgsLoading) {
    return (
      <CenteredCard>
        <h1 className="text-lg font-semibold text-high">Hosts</h1>
        <p className="mt-base text-sm text-normal">Loading...</p>
      </CenteredCard>
    );
  }

  if (displayError) {
    return (
      <CenteredCard>
        <h1 className="text-lg font-semibold text-high">Failed to load</h1>
        <p className="mt-base text-sm text-normal">{displayError}</p>
        <button
          type="button"
          className="mt-double rounded-sm bg-brand px-base py-half text-sm font-medium text-on-brand transition-colors hover:bg-brand-hover"
          onClick={() => {
            void handleSignInAgain();
          }}
        >
          Sign in again
        </button>
      </CenteredCard>
    );
  }

  return (
    <div className="h-full overflow-auto">
      <div className="mx-auto w-full max-w-3xl px-base py-base sm:px-double sm:py-double">
        <header className="space-y-half">
          <h1 className="text-2xl font-semibold text-high">Your Hosts</h1>
          <p className="text-sm text-low">
            Open a linked host to work on its workspaces.
          </p>
        </header>

        <section className="mt-double">
          {hosts.length === 0 ? (
            <div className="rounded-sm border border-border bg-secondary p-base text-center">
              <p className="text-sm text-low">No hosts linked yet</p>
              <button
                type="button"
                className="mt-base rounded-sm border border-border bg-primary px-base py-half text-sm font-medium text-normal hover:border-brand/60 hover:text-high"
                onClick={() => {
                  openRelaySettings();
                }}
              >
                Link a host
              </button>
            </div>
          ) : (
            <div className="space-y-half">
              {hosts.map((host) => {
                const isOnline = host.status === "online";
                const isUnpaired = host.status === "unpaired";
                const isClickable = isOnline || isUnpaired;

                return (
                  <button
                    key={host.id}
                    type="button"
                    disabled={!isClickable}
                    className={`flex w-full items-center gap-base rounded-sm border border-border bg-primary px-base py-base text-left transition-colors ${
                      isClickable
                        ? "hover:border-high/20 hover:bg-panel"
                        : "opacity-50"
                    }`}
                    onClick={() => {
                      if (isOnline) {
                        navigate({
                          to: "/hosts/$hostId/workspaces",
                          params: { hostId: host.id },
                        });
                      } else if (isUnpaired) {
                        openRelaySettings(host.id);
                      }
                    }}
                  >
                    <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-brand/15 text-xs font-semibold text-brand">
                      {getHostInitials(host.name)}
                    </div>
                    <span className="min-w-0 flex-1 truncate text-sm font-medium text-high">
                      {host.name}
                    </span>
                    <span
                      className={`h-2.5 w-2.5 shrink-0 rounded-full ${
                        isOnline
                          ? "bg-success"
                          : isUnpaired
                            ? "border border-warning bg-white"
                            : "bg-low"
                      }`}
                    />
                  </button>
                );
              })}
              <button
                type="button"
                className="flex w-full items-center justify-center rounded-sm border border-dashed border-border px-base py-half text-sm text-low hover:border-brand/60 hover:text-normal"
                onClick={() => {
                  openRelaySettings();
                }}
              >
                Link a host
              </button>
            </div>
          )}
        </section>
      </div>
    </div>
  );
}

function CenteredCard({ children }: { children: ReactNode }) {
  return (
    <div className="flex h-full items-center justify-center px-base">
      <section className="w-full max-w-md rounded-sm border border-border bg-secondary p-double text-center">
        {children}
      </section>
    </div>
  );
}
