import type { QueryClient } from '@tanstack/react-query';
import type {
  AnswerInput,
  InputDetail,
  InputPage,
  WorkspaceInput,
  InputReceipt,
  InputStatusWrite,
  SnoozeInput,
} from 'shared/types';
import {
  localApiDocumentUrl,
  makeLocalApiRequest,
} from '@/shared/lib/localApiTransport';

export async function refreshInputAfterConflict(
  error: unknown,
  refresh: () => Promise<void>
): Promise<boolean> {
  if (
    !(error instanceof Error) ||
    !/^(VERSION_CONFLICT|SEQUENCE_CONFLICT)(:|$)/.test(error.message)
  )
    return false;
  await refresh();
  return true;
}

export async function inputRequest<T>(
  workspace: string,
  hostId: string | null,
  suffix = '',
  body?: AnswerInput | InputStatusWrite | SnoozeInput | { priority: string }
): Promise<T> {
  const response = await makeLocalApiRequest(
    `/api/workspaces/${workspace}/inputs${suffix}`,
    {
      hostScope: 'explicit',
      hostId,
      method: body ? 'POST' : 'GET',
      headers: { 'Content-Type': 'application/json' },
      body: body ? JSON.stringify(body) : undefined,
    }
  );
  const envelope = await response.json();
  if (!response.ok || !envelope.success)
    throw new Error(
      envelope.message ?? `Input request failed (${response.status})`
    );
  return envelope.data as T;
}

export const inputsApi = {
  list: (
    workspace: string,
    host: string | null,
    showDone: boolean,
    cursor = ''
  ) =>
    inputRequest<InputPage>(
      workspace,
      host,
      `?limit=50&status=${showDone ? 'answered,closed,cancelled' : 'open'}&cursor=${encodeURIComponent(cursor)}`
    ),
  get: (workspace: string, host: string | null, id: string) =>
    inputRequest<InputDetail>(workspace, host, `/${id}`),
  priority: (
    workspace: string,
    host: string | null,
    id: string,
    priority: string
  ) =>
    inputRequest<WorkspaceInput>(workspace, host, `/${id}/priority`, {
      priority,
    }),
  contentUrl: (
    workspace: string,
    host: string | null,
    detail: { input: { id: string }; version: { version: number } }
  ) =>
    localApiDocumentUrl(
      `/api/workspaces/${workspace}/inputs/${detail.input.id}/versions/${detail.version.version}/content`,
      { hostScope: 'explicit', hostId: host }
    ),
  answer: (
    workspace: string,
    host: string | null,
    id: string,
    body: AnswerInput
  ) => inputRequest<InputReceipt>(workspace, host, `/${id}/answer`, body),
  snooze: (
    workspace: string,
    host: string | null,
    id: string,
    body: SnoozeInput
  ) => inputRequest<InputReceipt>(workspace, host, `/${id}/snooze`, body),
  status: (
    workspace: string,
    host: string | null,
    id: string,
    sequence: number,
    undo: boolean,
    requestId: string
  ) =>
    inputRequest<InputReceipt>(
      workspace,
      host,
      `/${id}/${undo ? 'undo-close' : 'close'}`,
      {
        expected_sequence: sequence,
        request_id: requestId,
        status: 'closed',
        reason: null,
      }
    ),
};

// ponytail: pending queues are small; load every page, use server sorting if queues grow large.
export async function loadPendingInputs(
  workspace: string,
  host: string | null
): Promise<InputPage> {
  const first = await inputsApi.list(workspace, host, false);
  const inputs = [...first.inputs];
  for (let cursor = first.next_cursor; cursor; ) {
    const page = await inputsApi.list(workspace, host, false, cursor);
    inputs.push(...page.inputs);
    cursor = page.next_cursor;
  }
  return { ...first, inputs, next_cursor: null };
}

export async function setInputPriority(
  queryClient: QueryClient,
  workspace: string,
  host: string | null,
  inputId: string,
  priority: string
): Promise<void> {
  await inputsApi.priority(workspace, host, inputId, priority);
  await Promise.all([
    queryClient.invalidateQueries({
      queryKey: ['input', host, workspace, inputId],
      exact: true,
    }),
    queryClient.invalidateQueries({ queryKey: ['inputs', host, workspace] }),
  ]);
}

export async function setInputSchedule(
  queryClient: QueryClient,
  workspace: string,
  host: string | null,
  inputId: string,
  body: SnoozeInput
): Promise<InputReceipt> {
  const receipt = await inputsApi.snooze(workspace, host, inputId, body);
  await Promise.all([
    queryClient.invalidateQueries({
      queryKey: ['input', host, workspace, inputId],
      exact: true,
    }),
    queryClient.invalidateQueries({ queryKey: ['inputs', host, workspace] }),
  ]);
  return receipt;
}
