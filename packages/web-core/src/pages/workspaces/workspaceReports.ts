import { makeLocalApiRequest } from '@/shared/lib/localApiTransport';

// API-local types until the backend's shared type generation lands.
export type ReportCell = {
  text: string;
  tone?: 'ok' | 'warn' | 'bad' | 'none';
  url?: string;
};
export type ReportTab = {
  id: string;
  title: string;
  columns: string[];
  rows: ReportCell[][];
  created_at: string;
  updated_at: string;
};
export type ReportTabs = { tabs: ReportTab[]; updated_at: string | null };
export type ReportScratchState = {
  scratch: {
    deleted?: boolean;
    payload: { type: 'WORKSPACE_REPORTS'; data: ReportTabs };
  } | null;
};

export const emptyReportScratch = (): ReportScratchState => ({ scratch: null });

export function reportSnapshot(state: ReportScratchState): ReportTabs {
  return state.scratch && !state.scratch.deleted
    ? state.scratch.payload.data
    : { tabs: [], updated_at: null };
}

export async function loadReportTabs(
  workspaceId: string,
  hostId: string | null,
  signal?: AbortSignal
): Promise<ReportTabs> {
  const response = await makeLocalApiRequest(
    `/api/workspaces/${encodeURIComponent(workspaceId)}/report-tabs`,
    { hostScope: 'explicit', hostId, signal }
  );
  const envelope = await response.json();
  if (!response.ok || !envelope.success)
    throw new Error(envelope.message ?? 'Could not load reports.');
  return envelope.data;
}

export function reportLink(value?: string): string | undefined {
  if (
    !value ||
    /[\s\\]/.test(value) ||
    Array.from(value).some(
      (char) => char.charCodeAt(0) < 32 || char.charCodeAt(0) === 127
    )
  )
    return;
  try {
    const url = new URL(value);
    if (
      /^https?:$/.test(url.protocol) &&
      url.hostname &&
      !url.username &&
      !url.password
    )
      return url.href;
  } catch {
    // Invalid URLs retain their plain text cell.
  }
}

export function selectedReportTab(tabs: ReportTab[], id: string | null) {
  return tabs.find((tab) => tab.id === id) ?? tabs[0];
}

export function nextReportTab(key: string, index: number, count: number) {
  if (!count) return;
  if (key === 'Home') return 0;
  if (key === 'End') return count - 1;
  if (key === 'ArrowRight') return (index + 1) % count;
  if (key === 'ArrowLeft') return (index - 1 + count) % count;
}
