import { useEffect } from 'react';
import { useUserSystem } from '@/shared/hooks/useUserSystem';
import { useAppNavigation } from '@/shared/hooks/useAppNavigation';

export function RootRedirectPage() {
  const { config, loading } = useUserSystem();
  const appNavigation = useAppNavigation();

  useEffect(() => {
    if (loading || !config) {
      return;
    }

    if (!config.remote_onboarding_acknowledged) {
      appNavigation.goToOnboarding({ replace: true });
      return;
    }

    appNavigation.goToWorkspaces({ replace: true });
  }, [appNavigation, config, loading]);

  return (
    <div className="h-screen bg-primary flex items-center justify-center">
      <p className="text-low">Loading...</p>
    </div>
  );
}
