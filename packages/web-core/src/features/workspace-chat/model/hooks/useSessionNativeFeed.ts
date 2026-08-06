import { useCallback, useMemo } from 'react';
import type {
  NativeFeedEntry,
  NativeFeedFork,
  NativeFeedSnapshot,
} from 'shared/types';

import { useHostId } from '@/shared/providers/HostIdProvider';
import { useJsonPatchWsStream } from '@/shared/hooks/useJsonPatchWsStream';
import { useUserSystem } from '@/shared/hooks/useUserSystem';

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
      foreign_writer_seen_at: null,
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
 *
 * Gated on the server's `cli_handover_enabled` flag. The feature ships dark
 * because the server rebuilds and re-serializes a session's ENTIRE transcript
 * on every appended line, and this hook then re-runs an Immer produce plus a
 * full sort copy over the result — an O(n²) cost in transcript length, paid
 * once per connected tab. While the flag is off no socket is opened at all: the
 * hook reports a stable empty snapshot and the conversation renders
 * executor-only.
 */
export function useSessionNativeFeed(
  sessionId: string | undefined
): UseSessionNativeFeedResult {
  const hostId = useHostId();
  const { cliHandoverEnabled } = useUserSystem();
  const enabled = Boolean(sessionId) && cliHandoverEnabled;
  const endpoint =
    sessionId && cliHandoverEnabled
      ? `${hostId ? `/api/host/${hostId}` : '/api'}/sessions/${sessionId}/native-feed/ws`
      : undefined;
  const initialData = useCallback(createEmptyNativeFeedSnapshot, []);

  const { data, isConnected, isInitialized, error } =
    useJsonPatchWsStream<NativeFeedSnapshot>(endpoint, enabled, initialData);

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
