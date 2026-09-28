import { useCallback } from 'react';
import type {
  NativeFeedEntry,
  NativeFeedFork,
  NativeFeedSnapshot,
} from 'shared/types';

import { useHostId } from '@/shared/providers/HostIdProvider';
import { useJsonPatchWsStream } from '@/shared/hooks/useJsonPatchWsStream';
import { useUserSystem } from '@/shared/hooks/useUserSystem';

const STREAM_OPTIONS = { resyncOnPatchFailure: true };
const EMPTY_ENTRIES: NativeFeedEntry[] = [];

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

/**
 * Session-scoped canonical Claude transcript feed.
 *
 * The server sends one full snapshot, then per update only the appended and
 * in-place replaced entries at their exact indexes. Entries arrive in `seq`
 * order. If a patch ever fails to apply, the stream reconnects for a fresh
 * snapshot instead of rendering a drifted copy.
 *
 * Gated on the server's `cli_transcript_ingest_enabled` flag: while the ingest
 * is off no socket is opened, the hook reports a stable empty snapshot, and the
 * conversation renders executor-only.
 */
export function useSessionNativeFeed(
  sessionId: string | undefined
): UseSessionNativeFeedResult {
  const hostId = useHostId();
  const { cliTranscriptIngestEnabled } = useUserSystem();
  const enabled = Boolean(sessionId) && cliTranscriptIngestEnabled;
  const endpoint =
    sessionId && cliTranscriptIngestEnabled
      ? `${hostId ? `/api/host/${hostId}` : '/api'}/sessions/${sessionId}/native-feed/ws`
      : undefined;
  const initialData = useCallback(createEmptyNativeFeedSnapshot, []);

  const { data, isConnected, isInitialized, error } =
    useJsonPatchWsStream<NativeFeedSnapshot>(
      endpoint,
      enabled,
      initialData,
      STREAM_OPTIONS
    );

  return {
    snapshot: data,
    entries: data?.entries ?? EMPTY_ENTRIES,
    forks: data?.forks ?? [],
    revision: data?.revision,
    isLoading: Boolean(sessionId) && !isInitialized && !error,
    isConnected,
    error,
  };
}
