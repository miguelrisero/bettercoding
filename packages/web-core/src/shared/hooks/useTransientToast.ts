import { useCallback, useEffect, useRef, useState } from 'react';

export type TransientToastTone = 'info' | 'success' | 'warning' | 'error';

export interface TransientToastState {
  id: number;
  message: string;
  tone: TransientToastTone;
}

export function useTransientToast(durationMs = 5_000) {
  const sequenceRef = useRef(0);
  const [toast, setToast] = useState<TransientToastState | null>(null);

  useEffect(() => {
    if (!toast) return;
    const timeout = window.setTimeout(() => {
      setToast((current) => (current?.id === toast.id ? null : current));
    }, durationMs);
    return () => window.clearTimeout(timeout);
  }, [durationMs, toast]);

  const showToast = useCallback(
    (message: string, tone: TransientToastTone = 'info') => {
      sequenceRef.current += 1;
      setToast({ id: sequenceRef.current, message, tone });
    },
    []
  );

  const dismissToast = useCallback(() => setToast(null), []);

  return { toast, showToast, dismissToast };
}
