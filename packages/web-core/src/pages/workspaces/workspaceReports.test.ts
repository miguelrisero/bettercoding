import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { setLocalApiTransport } from '@/shared/lib/localApiTransport';
import {
  loadReportTabs,
  nextReportTab,
  reportLink,
  selectedReportTab,
  type ReportTab,
} from './workspaceReports';
import { ReportTable } from './WorkspaceReportsContainer';

vi.mock('@/shared/hooks/useJsonPatchWsStream', () => ({
  useJsonPatchWsStream: vi.fn(),
}));

const tab: ReportTab = {
  id: 'checks',
  title: 'Checks <script>',
  columns: ['Check', 'Result'],
  rows: [
    [
      { text: '<img src=x onerror=alert(1)>' },
      { text: 'Passed', tone: 'ok', url: 'https://example.com/check' },
    ],
    [
      { text: '[markdown](javascript:alert(1))' },
      { text: 'Blocked', tone: 'bad', url: 'javascript:alert(1)' },
    ],
  ],
  created_at: '2026-10-29T12:00:00Z',
  updated_at: '2026-10-29T12:00:00Z',
};

afterEach(() => setLocalApiTransport(null));

describe('report requests', () => {
  it('routes each request to its explicit host and unwraps the envelope', async () => {
    const data = { tabs: [tab], updated_at: tab.updated_at };
    const request = vi.fn(
      async () => new Response(JSON.stringify({ success: true, data }))
    );
    setLocalApiTransport({ request, openWebSocket: vi.fn() });
    const controller = new AbortController();
    expect(
      await loadReportTabs('workspace', 'host-a', controller.signal)
    ).toEqual(data);
    expect(request).toHaveBeenLastCalledWith(
      '/api/host/host-a/workspaces/workspace/report-tabs',
      {
        hostScope: 'explicit',
        hostId: 'host-a',
        signal: controller.signal,
      }
    );
    await loadReportTabs('workspace', null);
    expect(request).toHaveBeenLastCalledWith(
      '/api/workspaces/workspace/report-tabs',
      {
        hostScope: 'explicit',
        hostId: null,
        signal: undefined,
      }
    );
  });

  it('reports API failures', async () => {
    setLocalApiTransport({
      request: vi.fn(
        async () =>
          new Response(
            JSON.stringify({ success: false, message: 'Reports unavailable' }),
            { status: 503 }
          )
      ),
      openWebSocket: vi.fn(),
    });
    await expect(loadReportTabs('workspace', null)).rejects.toThrow(
      'Reports unavailable'
    );
  });
});

describe('report text tables', () => {
  it('renders literal text, scoped headers, safe links, and visible tone labels', () => {
    const html = renderToStaticMarkup(createElement(ReportTable, { tab }));
    expect(html).toContain('&lt;img src=x onerror=alert(1)&gt;');
    expect(html).not.toContain('<img');
    expect(html).toContain('[markdown](javascript:alert(1))');
    expect(html).toContain('scope="col"');
    expect(html).toContain('href="https://example.com/check"');
    expect(html).toContain('rel="noopener noreferrer"');
    expect(html).not.toContain('href="javascript:');
    expect(html).toContain('OK');
    expect(html).toContain('Problem');
    expect(html).toContain('table-fixed');
    expect(html).toContain('overflow-auto');
    expect(html).toContain('sticky');
  });

  it('shows the empty table state', () => {
    expect(
      renderToStaticMarkup(
        createElement(ReportTable, { tab: { ...tab, rows: [] } })
      )
    ).toContain('No rows yet.');
  });
});

describe('report links', () => {
  it.each([
    'https://example.com/path?q=1#result',
    'http://localhost:3000/check',
    'https://[::1]/check',
  ])('accepts an absolute HTTP link: %s', (url) => {
    expect(reportLink(url)).toBe(url);
  });

  it.each([
    'javascript:alert(1)',
    'data:text/html,<script>alert(1)</script>',
    'file:///etc/passwd',
    '//example.com/path',
    '/api/workspaces',
    'https://',
    'https://user:secret@example.com/',
    'https://example.com\\@evil.com',
    'https://example.com\n/path',
    'https://bad host/',
    ' https://example.com/',
  ])('leaves an unsafe link as text: %s', (url) => {
    expect(reportLink(url)).toBeUndefined();
  });
});

describe('report tab selection', () => {
  const tabs = [tab, { ...tab, id: 'deploy' }, { ...tab, id: 'review' }];
  it('keeps selection by id and falls back after deletion or full replacement', () => {
    expect(selectedReportTab(tabs, 'deploy')?.id).toBe('deploy');
    expect(
      selectedReportTab(
        tabs.filter((t) => t.id !== 'deploy'),
        'deploy'
      )?.id
    ).toBe('checks');
    expect(
      selectedReportTab([{ ...tab, id: 'replacement' }], 'deploy')?.id
    ).toBe('replacement');
    expect(selectedReportTab([], 'deploy')).toBeUndefined();
  });

  it('supports wrapping arrow keys and Home/End', () => {
    expect(nextReportTab('ArrowRight', 2, 3)).toBe(0);
    expect(nextReportTab('ArrowLeft', 0, 3)).toBe(2);
    expect(nextReportTab('Home', 1, 3)).toBe(0);
    expect(nextReportTab('End', 0, 3)).toBe(2);
    expect(nextReportTab('Enter', 0, 3)).toBeUndefined();
    expect(nextReportTab('ArrowRight', 0, 0)).toBeUndefined();
  });
});
