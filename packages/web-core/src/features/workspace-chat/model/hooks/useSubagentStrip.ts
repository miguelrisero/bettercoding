import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';

import { useEntries } from '../contexts/EntriesContext';
import {
  partitionStrip,
  selectSubagents,
  type StripPartition,
  type SubagentDescriptor,
} from '../subagent-strip-model';

export const MAX_ACTIVE_TABS = 4;
export const FINISHED_LINGER_MS = 5_000;

export type UseSubagentStripResult = StripPartition & {
  hasAny: boolean;
};

export function useSubagentStrip(): UseSubagentStripResult {
  const { entries } = useEntries();
  const descriptors = useMemo(() => selectSubagents(entries), [entries]);
  const phasesByKeyRef = useRef<Record<string, SubagentDescriptor['phase']>>(
    {}
  );
  const doneAtByKeyRef = useRef<Record<string, number>>({});
  const [doneAtByKey, setDoneAtByKey] = useState<Record<string, number>>({});
  const [now, setNow] = useState(() => Date.now());

  useLayoutEffect(() => {
    const currentKeys = new Set(
      descriptors.map((descriptor) => descriptor.key)
    );
    const previousDoneAtByKey = doneAtByKeyRef.current;
    let nextDoneAtByKey = previousDoneAtByKey;
    let hasDoneAtChanges = false;

    const makeDoneAtMutable = () => {
      if (!hasDoneAtChanges) {
        nextDoneAtByKey = { ...previousDoneAtByKey };
        hasDoneAtChanges = true;
      }
    };

    for (const key of Object.keys(previousDoneAtByKey)) {
      if (!currentKeys.has(key)) {
        makeDoneAtMutable();
        delete nextDoneAtByKey[key];
      }
    }

    const nextPhasesByKey: Record<string, SubagentDescriptor['phase']> = {};
    let transitionTime: number | null = null;

    for (const descriptor of descriptors) {
      const previousPhase = phasesByKeyRef.current[descriptor.key];
      nextPhasesByKey[descriptor.key] = descriptor.phase;

      if (
        descriptor.phase !== 'active' &&
        previousPhase === 'active' &&
        nextDoneAtByKey[descriptor.key] === undefined
      ) {
        transitionTime ??= Date.now();
        makeDoneAtMutable();
        nextDoneAtByKey[descriptor.key] = transitionTime;
      }
    }

    phasesByKeyRef.current = nextPhasesByKey;

    if (hasDoneAtChanges) {
      doneAtByKeyRef.current = nextDoneAtByKey;
      setDoneAtByKey(nextDoneAtByKey);
    }
    if (transitionTime !== null) {
      setNow(transitionTime);
    }
  }, [descriptors]);

  useEffect(() => {
    let soonestExpiry: number | null = null;

    for (const descriptor of descriptors) {
      if (descriptor.phase === 'active') continue;

      const doneAt = doneAtByKey[descriptor.key];
      if (doneAt === undefined || now - doneAt >= FINISHED_LINGER_MS) {
        continue;
      }

      const expiry = doneAt + FINISHED_LINGER_MS;
      soonestExpiry =
        soonestExpiry === null ? expiry : Math.min(soonestExpiry, expiry);
    }

    if (soonestExpiry === null) return;

    let cancelled = false;
    const timeoutId = setTimeout(
      () => {
        if (!cancelled) {
          setNow(Date.now());
        }
      },
      Math.max(0, soonestExpiry - now)
    );

    return () => {
      cancelled = true;
      clearTimeout(timeoutId);
    };
  }, [descriptors, doneAtByKey, now]);

  const partition = useMemo(
    () =>
      partitionStrip({
        descriptors,
        doneAtByKey,
        now,
        maxActiveTabs: MAX_ACTIVE_TABS,
        lingerMs: FINISHED_LINGER_MS,
      }),
    [descriptors, doneAtByKey, now]
  );

  return {
    ...partition,
    hasAny: descriptors.length > 0,
  };
}
