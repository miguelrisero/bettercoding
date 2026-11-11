import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { describe, expect, it, vi } from 'vitest';
import { inputNavigationIsCurrent, nextOpenInput } from './inputFlow';

const source = readFileSync(
  new URL('./WorkspaceInputsContainer.tsx', import.meta.url),
  'utf8'
);

function closeFlow() {
  // Execute the production handlers with controlled persistence and navigation.
  const advanceSource = source
    .slice(
      source.indexOf('  const advance ='),
      source.indexOf('  const openInputs =')
    )
    .replace('fromId: string', 'fromId')
    .replace('let inputs: InputDetail[];', 'let inputs;')
    .replace('getQueryData<InputDetail>', 'getQueryData');
  const statusSource = source
    .slice(source.indexOf('  const status ='), source.indexOf('  const schedule ='))
    .replace('undo: boolean', 'undo')
    .replace('undoSequence!', 'undoSequence');
  const current = {
    input: {
      id: 'a',
      status: 'closed',
      priority: 'P2',
      created_at: '2026-11-01T00:00:00Z',
    },
  };
  const next = { input: { ...current.input, id: 'b', status: 'open' } };
  const session = { current: 1 };
  const mounted = { current: true };
  const persist = vi.fn(async () => ({ sequence: 2 }));
  const refresh = vi.fn(async () => {});
  const fetchQuery = vi.fn(async () => ({ inputs: [next] }));
  const open = vi.fn();
  const clearInputHash = vi.fn();
  const setSelected = vi.fn();
  const setDone = vi.fn();
  const setNotice = vi.fn();
  const setError = vi.fn();
  const setBusy = vi.fn();
  const setUndoSequence = vi.fn();
  const statusRequest = { current: null };
  const refreshInputAfterConflict = vi.fn(async () => false);
  const advance = runInNewContext(`${advanceSource}\nadvance;`, {
    session,
    queryClient: { fetchQuery, getQueryData: () => current },
    listQuery: {},
    host: null,
    workspaceId: 'workspace',
    selectedInput: () => 'a',
    inputNavigationIsCurrent,
    nextOpenInput,
    useUiPreferencesStore: { getState: () => ({ inputSort: 'priority' }) },
    open,
    clearInputHash,
    setSelected,
    setDone,
    setNotice,
  });
  const onClosed = vi.fn(async () => advance('a', 'Input closed'));
  const status = runInNewContext(`${statusSource}\nstatus;`, {
    inputsApi: { status: persist },
    workspaceId: 'workspace',
    host: null,
    detail: { input: { id: 'a', sequence: 1 } },
    undoSequence: 2,
    statusRequest,
    crypto: { randomUUID: () => 'request-id' },
    refresh,
    refreshAfterConflict: vi.fn(),
    refreshInputAfterConflict,
    mounted,
    onClosed,
    setError,
    setBusy,
    setUndoSequence,
  }) as (undo: boolean) => Promise<void>;
  return {
    status,
    persist,
    refresh,
    fetchQuery,
    mounted,
    session,
    onClosed,
    open,
    clearInputHash,
    setSelected,
    setDone,
    setNotice,
    setError,
    setBusy,
    setUndoSequence,
  };
}

describe('Close Input navigation', () => {
  it('passes the parent close callback through the detail and body', () => {
    const viewer = source.slice(
      source.indexOf('export function WorkspaceInputViewer'),
      source.indexOf('const BURST')
    );
    const callback = viewer.match(/onClosed=\{\(\) => ([^\n]+)\}/)?.[1];
    const advance = vi.fn();
    expect(callback).toBeDefined();
    runInNewContext(callback!, { advance, selected: 'a' });
    expect(advance).toHaveBeenCalledWith('a', 'Input closed');
    const detail = source.slice(
      source.indexOf('function InputViewer('),
      source.indexOf('function InputBody(')
    );
    expect(detail).toContain('onClosed={onClosed}');
  });

  it('refreshes a persisted close before advancing to the next open Input', async () => {
    const flow = closeFlow();
    flow.refresh.mockImplementation(async () => {
      expect(flow.persist).toHaveBeenCalledWith(
        'workspace',
        null,
        'a',
        1,
        false,
        'request-id'
      );
      expect(flow.onClosed).not.toHaveBeenCalled();
    });
    await flow.status(false);
    expect(flow.refresh).toHaveBeenCalledOnce();
    expect(flow.onClosed).toHaveBeenCalledOnce();
    expect(flow.open).toHaveBeenCalledWith('b');
    expect(flow.setNotice).toHaveBeenCalledWith('Input closed');
    expect(flow.setUndoSequence).toHaveBeenCalledWith(2);
    expect(flow.setBusy).toHaveBeenLastCalledWith(false);
  });

  it('shows all done when the persisted close leaves no open Inputs', async () => {
    const flow = closeFlow();
    flow.fetchQuery.mockResolvedValue({ inputs: [] });
    await flow.status(false);
    expect(flow.open).not.toHaveBeenCalled();
    expect(flow.clearInputHash).toHaveBeenCalledOnce();
    expect(flow.setSelected).toHaveBeenCalledWith(null);
    expect(flow.setDone).toHaveBeenCalledWith(true);
  });

  it('keeps Undo close in the current viewer', async () => {
    const flow = closeFlow();
    await flow.status(true);
    expect(flow.persist).toHaveBeenCalledWith(
      'workspace',
      null,
      'a',
      2,
      true,
      'request-id'
    );
    expect(flow.refresh).toHaveBeenCalledOnce();
    expect(flow.setUndoSequence).toHaveBeenCalledWith(null);
    expect(flow.onClosed).not.toHaveBeenCalled();
  });

  it.each(['persist', 'refresh'] as const)(
    'keeps the viewer when %s fails',
    async (operation) => {
      const flow = closeFlow();
      flow[operation].mockRejectedValue(new Error('save failed'));
      await flow.status(false);
      expect(flow.setError).toHaveBeenLastCalledWith('Error: save failed');
      expect(flow.onClosed).not.toHaveBeenCalled();
      expect(flow.open).not.toHaveBeenCalled();
    }
  );

  it('ignores a close that finishes after the body unmounts', async () => {
    const flow = closeFlow();
    flow.refresh.mockImplementation(async () => {
      flow.mounted.current = false;
    });
    await flow.status(false);
    expect(flow.onClosed).not.toHaveBeenCalled();
    expect(flow.open).not.toHaveBeenCalled();
  });

  it('keeps the viewer when the pending list refresh fails', async () => {
    const flow = closeFlow();
    flow.fetchQuery.mockRejectedValue(new Error('refresh failed'));
    await flow.status(false);
    expect(flow.onClosed).toHaveBeenCalledOnce();
    expect(flow.open).not.toHaveBeenCalled();
    expect(flow.setDone).not.toHaveBeenCalled();
  });

  it('ignores navigation when the viewer session changes during list refresh', async () => {
    const flow = closeFlow();
    flow.fetchQuery.mockImplementation(async () => {
      flow.session.current += 1;
      return { inputs: [] };
    });
    await flow.status(false);
    expect(flow.onClosed).toHaveBeenCalledOnce();
    expect(flow.open).not.toHaveBeenCalled();
    expect(flow.clearInputHash).not.toHaveBeenCalled();
    expect(flow.setDone).not.toHaveBeenCalled();
  });
});
