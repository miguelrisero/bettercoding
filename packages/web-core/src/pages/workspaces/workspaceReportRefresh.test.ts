import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ReportTabs } from 'shared/types';
import { beforeEach, expect, it, vi } from 'vitest';
import { WorkspaceReportsContainer } from './WorkspaceReportsContainer';

const state = vi.hoisted(() => ({
  live: undefined as ReportTabs | undefined,
  http: undefined as ReportTabs | undefined,
  error: null as Error | null,
  connected: true,
  retry: vi.fn(),
  reconnect: vi.fn(),
}));

vi.mock('react', async (importOriginal) => {
  const react = await importOriginal<typeof import('react')>();
  return {
    ...react,
    // SSR cannot run effects. Supply the snapshot retained by the stream effect.
    useState: (initial: unknown) =>
      initial === undefined ? [state.live, vi.fn()] : react.useState(initial),
  };
});
vi.mock('@tanstack/react-query', () => ({
  useQuery: () => ({
    data: state.http,
    error: state.error,
    refetch: state.retry,
  }),
}));
vi.mock('@/shared/providers/HostIdProvider', () => ({ useHostId: () => null }));
vi.mock('@/shared/hooks/useJsonPatchWsStream', () => ({
  useJsonPatchWsStream: () => ({
    isConnected: state.connected,
    isInitialized: state.connected,
    error: null,
    reconnect: state.reconnect,
  }),
}));

function snapshot(text: string, updated_at: string): ReportTabs {
  return {
    updated_at,
    tabs: [
      {
        id: 'checks',
        title: 'Checks',
        columns: ['Status'],
        rows: [[{ text }]],
        created_at: updated_at,
        updated_at,
      },
    ],
  };
}
const render = () =>
  renderToStaticMarkup(
    createElement(WorkspaceReportsContainer, { workspaceId: 'workspace' })
  );

beforeEach(() => {
  state.live = snapshot('Initial stream result', '2026-10-29T12:00:00Z');
  state.http = state.live;
  state.error = null;
  state.connected = true;
});

it('renders a newer HTTP retry after an initialized stream disconnects', () => {
  expect(render()).toContain('Initial stream result');
  state.connected = false;
  expect(render()).toContain('Live updates disconnected');
  state.http = snapshot('Fresh retry result', '2026-10-29T12:01:00Z');
  const html = render();
  expect(html).toContain('Fresh retry result');
  expect(html).not.toContain('Initial stream result');
});

it('keeps a newer stream snapshot when an older HTTP response arrives late', () => {
  state.live = snapshot('Latest stream result', '2026-10-29T12:02:00Z');
  state.http = snapshot('Delayed HTTP result', '2026-10-29T12:01:00Z');
  const html = render();
  expect(html).toContain('Latest stream result');
  expect(html).not.toContain('Delayed HTTP result');
});

it('shows HTTP retry errors beside the retained stream result', () => {
  state.connected = false;
  state.error = new Error('Reports temporarily unavailable');
  const html = render();
  expect(html).toContain('role="alert"');
  expect(html).toContain('Reports temporarily unavailable');
  expect(html).toContain('Initial stream result');
  expect(html).toContain('Retry');
});
