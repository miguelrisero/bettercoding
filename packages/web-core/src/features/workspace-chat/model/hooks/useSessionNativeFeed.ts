import { useCallback, useMemo } from 'react';
import type {
  NativeFeedEntry,
  NativeFeedFork,
  NativeFeedSnapshot,
} from 'shared/types';

import { useHostId } from '@/shared/providers/HostIdProvider';
import { useJsonPatchWsStream } from '@/shared/hooks/useJsonPatchWsStream';

export interface UseSessionNativeFeedResult {
  snapshot: NativeFeedSnapshot | undefined;
  entries: NativeFeedEntry[];
  forks: NativeFeedFork[];
  revision: bigint | undefined;
  isLoading: boolean;
  isConnected: boolean;
  error: string | null;
}

function createEmptyNativeFeedSnapshot(): NativeFeedSnapshot {
  return {
    revision: 0n,
    seq: 0n,
    entries: [],
    forks: [],
    health: {
      unknown_kinds: 0n,
      rescans: 0n,
      quarantined_files: 0n,
      watch_degraded: false,
      files: [],
    },
  };
}

function compareNativeEntries(
  left: NativeFeedEntry,
  right: NativeFeedEntry
): number {
  if (left.seq < right.seq) return -1;
  if (left.seq > right.seq) return 1;
  return 0;
}

/**
 * Session-scoped canonical Claude transcript feed.
 *
 * The server publishes every update as a revisioned top-level snapshot
 * replacement. Sorting a copy keeps the transport snapshot immutable while
 * preserving source order for entries produced from the same native record.
 */
export function useSessionNativeFeed(
  sessionId: string | undefined
): UseSessionNativeFeedResult {
  const hostId = useHostId();
  const endpoint = sessionId
    ? `${hostId ? `/api/host/${hostId}` : '/api'}/sessions/${sessionId}/native-feed/ws`
    : undefined;
  const initialData = useCallback(createEmptyNativeFeedSnapshot, []);

  const { data, isConnected, isInitialized, error } =
    useJsonPatchWsStream<NativeFeedSnapshot>(
      endpoint,
      Boolean(sessionId),
      initialData
    );

  const entries = useMemo(
    () => [...(data?.entries ?? [])].sort(compareNativeEntries),
    [data?.entries]
  );

  return {
    snapshot: data,
    entries,
    forks: data?.forks ?? [],
    revision: data?.revision,
    isLoading: Boolean(sessionId) && !isInitialized && !error,
    isConnected,
    error,
  };
}
