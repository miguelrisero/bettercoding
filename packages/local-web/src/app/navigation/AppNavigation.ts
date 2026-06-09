import { router } from '@web/app/router';
import type { FileRouteTypes } from '@web/routeTree.gen';
import {
  type AppDestination,
  type AppNavigation,
  type NavigationTransition,
} from '@/shared/lib/routes/appNavigation';

type LocalRouteId = FileRouteTypes['id'];

function getPathParam(
  routeParams: Record<string, string>,
  key: string
): string | null {
  const value = routeParams[key];
  return value ? value : null;
}

function parseLocalHostIdFromPathname(pathname: string): string | null {
  const segments = pathname.split('/').filter(Boolean);
  const hostsIndex = segments.indexOf('hosts');
  if (hostsIndex === -1) {
    return null;
  }
  return segments[hostsIndex + 1] ?? null;
}

function resolveLocalDestinationFromPath(path: string): AppDestination | null {
  const { pathname } = new URL(path, 'http://localhost');
  const { foundRoute, routeParams } = router.getMatchedRoutes(pathname);

  if (!foundRoute) {
    return null;
  }

  switch (foundRoute.id as LocalRouteId) {
    case '/':
      return { kind: 'root' };
    case '/onboarding':
      return { kind: 'onboarding' };
    case '/onboarding_/sign-in':
      return { kind: 'onboarding-sign-in' };
    case '/_app/workspaces':
      return { kind: 'workspaces' };
    case '/_app/hosts/$hostId/workspaces': {
      const hostId = getPathParam(routeParams, 'hostId');
      return hostId ? { kind: 'workspaces', hostId } : null;
    }
    case '/_app/workspaces_/create':
      return { kind: 'workspaces-create' };
    case '/_app/hosts/$hostId/workspaces_/create': {
      const hostId = getPathParam(routeParams, 'hostId');
      return hostId ? { kind: 'workspaces-create', hostId } : null;
    }
    case '/_app/workspaces_/$workspaceId': {
      const workspaceId = getPathParam(routeParams, 'workspaceId');
      return workspaceId ? { kind: 'workspace', workspaceId } : null;
    }
    case '/_app/hosts/$hostId/workspaces_/$workspaceId': {
      const hostId = getPathParam(routeParams, 'hostId');
      const workspaceId = getPathParam(routeParams, 'workspaceId');
      return hostId && workspaceId
        ? { kind: 'workspace', hostId, workspaceId }
        : null;
    }
    case '/workspaces/$workspaceId/vscode': {
      const workspaceId = getPathParam(routeParams, 'workspaceId');
      return workspaceId ? { kind: 'workspace-vscode', workspaceId } : null;
    }
    case '/hosts/$hostId/workspaces/$workspaceId/vscode': {
      const hostId = getPathParam(routeParams, 'hostId');
      const workspaceId = getPathParam(routeParams, 'workspaceId');
      return hostId && workspaceId
        ? { kind: 'workspace-vscode', hostId, workspaceId }
        : null;
    }
    default:
      return null;
  }
}

function destinationToLocalTarget(
  destination: AppDestination,
  options: { currentHostId: string | null }
) {
  const destinationHostId =
    'hostId' in destination ? (destination.hostId ?? null) : null;
  const effectiveHostId = destinationHostId ?? options.currentHostId;

  switch (destination.kind) {
    case 'root':
      return { to: '/' } as const;
    case 'onboarding':
      return { to: '/onboarding' } as const;
    case 'onboarding-sign-in':
      return { to: '/onboarding/sign-in' } as const;
    case 'workspaces':
      if (effectiveHostId) {
        return {
          to: '/hosts/$hostId/workspaces',
          params: { hostId: effectiveHostId },
        } as const;
      }
      return { to: '/workspaces' } as const;
    case 'workspaces-create':
      if (effectiveHostId) {
        return {
          to: '/hosts/$hostId/workspaces/create',
          params: { hostId: effectiveHostId },
        } as const;
      }
      return { to: '/workspaces/create' } as const;
    case 'workspace':
      if (effectiveHostId) {
        return {
          to: '/hosts/$hostId/workspaces/$workspaceId',
          params: {
            hostId: effectiveHostId,
            workspaceId: destination.workspaceId,
          },
        } as const;
      }
      return {
        to: '/workspaces/$workspaceId',
        params: { workspaceId: destination.workspaceId },
      } as const;
    case 'workspace-vscode':
      if (effectiveHostId) {
        return {
          to: '/hosts/$hostId/workspaces/$workspaceId/vscode',
          params: {
            hostId: effectiveHostId,
            workspaceId: destination.workspaceId,
          },
        } as const;
      }
      return {
        to: '/workspaces/$workspaceId/vscode',
        params: { workspaceId: destination.workspaceId },
      } as const;
  }
}

export function createLocalAppNavigation(): AppNavigation {
  const navigateTo = (
    destination: AppDestination,
    transition?: NavigationTransition
  ) => {
    const currentHostId =
      typeof window === 'undefined'
        ? null
        : parseLocalHostIdFromPathname(window.location.pathname);

    void router.navigate({
      ...destinationToLocalTarget(destination, { currentHostId }),
      ...(transition?.replace !== undefined
        ? { replace: transition.replace }
        : {}),
    });
  };

  const navigation: AppNavigation = {
    resolveFromPath: (path) => resolveLocalDestinationFromPath(path),
    goToRoot: (transition) => navigateTo({ kind: 'root' }, transition),
    goToOnboarding: (transition) =>
      navigateTo({ kind: 'onboarding' }, transition),
    goToOnboardingSignIn: (transition) =>
      navigateTo({ kind: 'onboarding-sign-in' }, transition),
    goToWorkspaces: (transition) =>
      navigateTo({ kind: 'workspaces' }, transition),
    goToWorkspacesCreate: (transition) =>
      navigateTo({ kind: 'workspaces-create' }, transition),
    goToWorkspace: (workspaceId, transition) =>
      navigateTo({ kind: 'workspace', workspaceId }, transition),
    goToWorkspaceVsCode: (workspaceId, transition) =>
      navigateTo({ kind: 'workspace-vscode', workspaceId }, transition),
  };

  return navigation;
}

export const localAppNavigation = createLocalAppNavigation();
