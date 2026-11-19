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
    .replace('closing?: ClosingInput', 'closing')
    .replace('let inputs: InputDetail[];', 'let inputs;')
    .replace('getQueryData<InputDetail>', 'getQueryData');
  const statusSource = source
    .slice(
      source.indexOf('  const status ='),
      source.indexOf('  const schedule =')
    )
    .replace('undo: boolean', 'undo')
    .replace('reason: CloseReason', 'reason')
    .replace('undoSequence!', 'undoSequence');
  const current = {
    input: {
      id: 'a',
      status: 'closed',
      priority: 'P2',
      created_at: '2026-11-01T00:00:00Z',
    },
  };
  const detail = { input: { ...current.input, status: 'open', sequence: 1 } };
  const next = { input: { ...current.input, id: 'b', status: 'open' } };
  const session = { current: 1 };
  const mounted = { current: true };
  const persist = vi.fn(async (..._args: unknown[]) => ({ sequence: 2 }));
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
  const focusDestination = { current: false };
  let requestCount = 0;
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
    focusDestination,
  });
  const onClosed = vi.fn(advance);
  const prepareClose = () => {
    const closing = {
      started: session.current,
      current: detail,
      sort: 'priority',
      order: ['a', 'b'],
    };
    return (reason: string | null) =>
      onClosed('a', reason ? 'Input marked as seen' : 'Input closed', closing);
  };
  const status = runInNewContext(`${statusSource}\nstatus;`, {
    inputsApi: { status: persist },
    workspaceId: 'workspace',
    host: null,
    detail,
    undoSequence: 2,
    statusRequest,
    statusSaving: { current: false },
    busy: false,
    crypto: { randomUUID: () => `request-${++requestCount}` },
    refresh,
    refreshAfterConflict: vi.fn(),
    refreshInputAfterConflict,
    mounted,
    prepareClose,
    setError,
    setBusy,
    setUndoSequence,
  }) as (undo: boolean, reason?: 'marked_as_seen') => Promise<void>;
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
    focusDestination,
  };
}

describe('Close Input navigation', () => {
  it('refreshes a persisted close before advancing to the next open Input', async () => {
    const flow = closeFlow();
    flow.refresh.mockImplementation(async () => {
      expect(flow.persist).toHaveBeenCalledWith(
        'workspace',
        null,
        'a',
        1,
        false,
        'request-1',
        null
      );
      expect(flow.onClosed).not.toHaveBeenCalled();
    });
    await flow.status(false);
    expect(flow.refresh).toHaveBeenCalledOnce();
    expect(flow.refresh).toHaveBeenCalledWith(true);
    expect(flow.onClosed).toHaveBeenCalledOnce();
    expect(flow.open).toHaveBeenCalledWith('b', true);
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
    expect(flow.focusDestination.current).toBe(true);
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
      'request-1',
      null
    );
    expect(flow.refresh).toHaveBeenCalledOnce();
    expect(flow.refresh).toHaveBeenCalledWith(false);
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

  it('binds each close retry key to its action', async () => {
    const flow = closeFlow();
    flow.persist.mockRejectedValue(new Error('network unavailable'));
    await flow.status(false);
    await flow.status(false, 'marked_as_seen');
    await flow.status(false, 'marked_as_seen');
    expect(flow.persist.mock.calls.map((call) => call.slice(-2))).toEqual([
      ['request-1', null],
      ['request-2', 'marked_as_seen'],
      ['request-2', 'marked_as_seen'],
    ]);
  });

  it('sends one acknowledgment while a save is in progress', async () => {
    const flow = closeFlow();
    await Promise.all([
      flow.status(false, 'marked_as_seen'),
      flow.status(false, 'marked_as_seen'),
    ]);
    expect(flow.persist).toHaveBeenCalledOnce();
    expect(flow.setNotice).toHaveBeenCalledWith('Input marked as seen');
  });

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

  it('ignores a close after the selection epoch changes during the save', async () => {
    const flow = closeFlow();
    flow.persist.mockImplementation(async () => {
      flow.session.current += 1;
      return { sequence: 2 };
    });
    await flow.status(false);
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
