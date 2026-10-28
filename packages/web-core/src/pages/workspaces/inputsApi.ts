import type {
  AnswerInput,
  InputDetail,
  InputPage,
  InputReceipt,
  InputStatusWrite,
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
  body?: AnswerInput | InputStatusWrite
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
