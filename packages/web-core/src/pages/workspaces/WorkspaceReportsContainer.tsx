import { useEffect, useId, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import type { ReportTab, ReportTabs } from 'shared/types';
import * as Dialog from '@radix-ui/react-dialog';
import { ArrowsOutSimpleIcon, XIcon } from '@phosphor-icons/react';
import { useHostId } from '@/shared/providers/HostIdProvider';
import { useJsonPatchWsStream } from '@/shared/hooks/useJsonPatchWsStream';
import { formatRelativeTime } from '@/shared/lib/date';
import { cn } from '@/shared/lib/utils';
import {
  emptyReportScratch,
  loadReportTabs,
  nextReportTab,
  reportLink,
  reportSnapshot,
  selectedReportTab,
} from './workspaceReports';

const focusRing =
  'focus:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand';
const tones = {
  ok: { label: 'OK', className: 'bg-success' },
  warn: { label: 'Warning', className: 'bg-brand' },
  bad: { label: 'Problem', className: 'bg-error' },
};

export function ReportTable({ tab }: { tab: ReportTab }) {
  return (
    <div
      className={`min-h-0 overflow-auto rounded border ${focusRing}`}
      role="region"
      aria-label={`${tab.title} table`}
      tabIndex={0}
    >
      <table
        className="w-full table-fixed text-base text-normal"
        style={{ minWidth: `${tab.columns.length * 132}px` }}
      >
        <caption className="sr-only">{tab.title}</caption>
        <thead className="sticky top-0 z-10 bg-primary text-high">
          <tr>
            {tab.columns.map((column, index) => (
              <th
                key={index}
                scope="col"
                className="border-b px-base py-half text-left align-top font-medium whitespace-pre-wrap [overflow-wrap:anywhere]"
              >
                {column}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="divide-y">
          {tab.rows.map((row, rowIndex) => (
            <tr key={rowIndex} className="even:bg-primary/40">
              {tab.columns.map((_, columnIndex) => {
                const cell = row[columnIndex];
                const href = reportLink(cell?.url);
                const tone =
                  cell?.tone && cell.tone !== 'none'
                    ? tones[cell.tone]
                    : undefined;
                return (
                  <td
                    key={columnIndex}
                    className="px-base py-base align-top whitespace-pre-wrap [overflow-wrap:anywhere]"
                  >
                    {href ? (
                      <a
                        href={href}
                        target="_blank"
                        rel="noopener noreferrer"
                        className={`underline underline-offset-2 hover:text-high ${focusRing}`}
                      >
                        {cell.text}
                        <span className="sr-only"> (opens in a new tab)</span>
                      </a>
                    ) : (
                      cell?.text
                    )}
                    {tone && (
                      <span className="mt-half flex items-center gap-1 text-sm text-low">
                        <span
                          aria-hidden
                          className={cn(
                            'size-1.5 shrink-0 rounded-full',
                            tone.className
                          )}
                        />
                        {tone.label}
                      </span>
                    )}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
      {tab.rows.length === 0 && <p className="p-base text-low">No rows yet.</p>}
    </div>
  );
}

export function WorkspaceReportsContainer({
  workspaceId,
}: {
  workspaceId: string;
}) {
  const hostId = useHostId();
  return (
    <Reports
      key={`${hostId ?? 'local'}:${workspaceId}`}
      workspaceId={workspaceId}
      hostId={hostId}
    />
  );
}

function Reports({
  workspaceId,
  hostId,
}: {
  workspaceId: string;
  hostId: string | null;
}) {
  const list = useQuery({
    queryKey: ['workspace-reports', hostId, workspaceId],
    queryFn: ({ signal }) => loadReportTabs(workspaceId, hostId, signal),
  });
  const stream = useJsonPatchWsStream(
    `/api/scratch/WORKSPACE_REPORTS/${encodeURIComponent(workspaceId)}/stream/ws`,
    true,
    emptyReportScratch,
    { hostScope: 'explicit', hostId, resyncOnPatchFailure: true }
  );
  // Retain the last complete snapshot while the stream reconnects.
  const [live, setLive] = useState<ReportTabs>();
  useEffect(() => {
    if (stream.isInitialized && stream.data)
      setLive(reportSnapshot(stream.data));
  }, [stream.data, stream.isInitialized]);
  const [selected, setSelected] = useState<string | null>(null);
  const tabs = live ?? list.data;
  const tab = selectedReportTab(tabs?.tabs ?? [], selected);
  const id = useId();
  const buttons = useRef<Array<HTMLButtonElement | null>>([]);
  const [, tick] = useState(0);
  useEffect(() => {
    const timer = window.setInterval(() => tick((value) => value + 1), 60000);
    return () => window.clearInterval(timer);
  }, []);
  const error =
    stream.error ?? (live === undefined ? list.error?.message : null);
  return (
    <section
      aria-label="Reports"
      className="flex w-full min-w-0 flex-col gap-base p-base overflow-hidden"
    >
      <div className="flex items-center justify-between gap-half text-sm text-low">
        <p role="status">
          {stream.isConnected ? 'Live' : 'Live updates disconnected'}
        </p>
        {(!stream.isConnected || error) && (
          <button
            className={`shrink-0 rounded px-half py-1 underline hover:text-high ${focusRing}`}
            onClick={() => {
              void list.refetch();
              stream.reconnect();
            }}
          >
            Retry
          </button>
        )}
      </div>
      {error && (
        <p role="alert" className="text-base text-error">
          {error}
        </p>
      )}
      {!tabs && !error && (
        <p role="status" className="text-low">
          Loading reports…
        </p>
      )}
      {tabs?.tabs.length === 0 && (
        <p className="text-low">Your workspace reports will appear here.</p>
      )}
      {tab && (
        <>
          <div
            role="tablist"
            aria-label="Workspace reports"
            className="flex shrink-0 flex-wrap gap-1"
          >
            {tabs!.tabs.map((item, index) => (
              <button
                key={item.id}
                ref={(element) => {
                  buttons.current[index] = element;
                }}
                id={`${id}-tab-${index}`}
                role="tab"
                aria-selected={item.id === tab.id}
                aria-controls={`${id}-panel-${index}`}
                tabIndex={item.id === tab.id ? 0 : -1}
                className={cn(
                  `max-w-full min-h-8 rounded border px-half py-1 text-base text-left [overflow-wrap:anywhere] ${focusRing}`,
                  item.id === tab.id
                    ? 'bg-primary text-high border-brand'
                    : 'text-low hover:bg-primary hover:text-high'
                )}
                onClick={() => setSelected(item.id)}
                onKeyDown={(event) => {
                  const next = nextReportTab(
                    event.key,
                    index,
                    tabs!.tabs.length
                  );
                  if (next === undefined) return;
                  event.preventDefault();
                  setSelected(tabs!.tabs[next].id);
                  buttons.current[next]?.focus();
                }}
              >
                {item.title}
              </button>
            ))}
          </div>
          {tabs!.tabs.map((item, index) => (
            <div
              key={item.id}
              id={`${id}-panel-${index}`}
              role="tabpanel"
              aria-labelledby={`${id}-tab-${index}`}
              hidden={item.id !== tab.id}
              className={
                item.id === tab.id
                  ? 'flex min-h-0 flex-col gap-half'
                  : undefined
              }
            >
              {item.id === tab.id && (
                <>
                  <div className="flex shrink-0 items-center justify-between gap-half text-sm text-low">
                    <p>
                      Updated{' '}
                      <time dateTime={item.updated_at} title={item.updated_at}>
                        {formatRelativeTime(item.updated_at)}
                      </time>
                    </p>
                    <Dialog.Root>
                      <Dialog.Trigger asChild>
                        <button
                          aria-label="Expand reports"
                          title="Expand reports"
                          className={`flex min-h-8 items-center gap-1 rounded px-half hover:bg-primary hover:text-high ${focusRing}`}
                        >
                          <ArrowsOutSimpleIcon aria-hidden className="size-4" />
                          Expand
                        </button>
                      </Dialog.Trigger>
                      <Dialog.Portal>
                        <Dialog.Overlay className="fixed inset-0 z-50 bg-black/60" />
                        <Dialog.Content className="new-design fixed inset-3 z-50 flex min-h-0 flex-col gap-base overflow-hidden rounded-lg border bg-primary p-base text-normal shadow-xl md:inset-12 md:p-double">
                          <div className="flex shrink-0 items-start justify-between gap-base">
                            <Dialog.Title className="min-w-0 text-xl font-semibold text-high [overflow-wrap:anywhere]">
                              {item.title}
                            </Dialog.Title>
                            <Dialog.Close
                              aria-label="Close reports"
                              className={`flex size-9 shrink-0 items-center justify-center rounded hover:bg-secondary ${focusRing}`}
                            >
                              <XIcon aria-hidden className="size-5" />
                            </Dialog.Close>
                          </div>
                          <Dialog.Description className="text-base text-low">
                            Workspace report · Updated{' '}
                            {formatRelativeTime(item.updated_at)}
                          </Dialog.Description>
                          <ReportTable tab={item} />
                        </Dialog.Content>
                      </Dialog.Portal>
                    </Dialog.Root>
                  </div>
                  <ReportTable tab={item} />
                </>
              )}
            </div>
          ))}
        </>
      )}
    </section>
  );
}
