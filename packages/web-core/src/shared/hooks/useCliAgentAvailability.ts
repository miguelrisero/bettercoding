import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import { configApi } from '@/shared/lib/api';
import type { BaseCodingAgent } from 'shared/types';

/**
 * Which agents' interactive CLI binary is on PATH. CLI mode launches the
 * agent's own binary, so the create-flow picker uses this to flag agents that
 * won't start in CLI mode (managed mode runs via npx and is unaffected).
 *
 * While the answer is still loading, everything reports as installed so the UI
 * never flashes a misleading "not installed".
 */
export function useCliAgentAvailability() {
  const { data } = useQuery({
    queryKey: ['cli-agent-availability'],
    queryFn: () => configApi.cliAgentAvailability(),
    staleTime: 60_000,
  });

  return useMemo(() => {
    const known = data !== undefined;
    const installed = new Set<BaseCodingAgent>(data?.installed ?? []);
    return {
      known,
      isInstalled: (agent: BaseCodingAgent) => !known || installed.has(agent),
    };
  }, [data]);
}
