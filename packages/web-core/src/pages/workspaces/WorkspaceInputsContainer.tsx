import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import * as Dialog from '@radix-ui/react-dialog';
import type { AnswerInput, InputAnswer, InputDetail } from 'shared/types';
import { useHostId } from '@/shared/providers/HostIdProvider';
import { useVisualViewportGeometry } from '@/shared/hooks/useVisualViewportGeometry';
import { useIsMobile } from '@/shared/hooks/useIsMobile';
import {
  inputEmbeddingDocument,
  inputFrameLoaded,
  createInputBridge,
  readInputProposal,
  validateInputAnswer,
  type InputBridge,
} from './inputBridge';
import { inputsApi, refreshInputAfterConflict } from './inputsApi';

const button =
  'min-h-11 px-base rounded border text-high focus-visible:ring-2 focus-visible:ring-brand disabled:opacity-50';

function selectedInput(): string | null {
  const value = new URLSearchParams(window.location.hash.slice(1)).get('input');
  return value && /^[0-9a-f-]{36}$/i.test(value) ? value : null;
}

export function WorkspaceInputsContainer({
  workspaceId,
}: {
  workspaceId: string;
}) {
  const host = useHostId();
  const [all, setAll] = useState(false);
  const [cursor, setCursor] = useState('');
  const list = useQuery({
    queryKey: ['inputs', host, workspaceId, all, cursor],
    queryFn: () => inputsApi.list(workspaceId, host, all, cursor),
    refetchInterval: 10000,
  });
  return (
    <section className="w-full p-base space-y-base" aria-label="Inputs">
      <div className="flex items-center justify-between gap-base">
        <strong>{list.data?.pending_count ?? 0} pending</strong>
        <button
          className={button}
          onClick={() => {
            setAll(!all);
            setCursor('');
          }}
        >
          {all ? 'Show pending' : 'Show all'}
        </button>
      </div>
      {list.isLoading && <p role="status">Loading Inputs…</p>}
      {list.error && <p role="alert">{list.error.message}</p>}
      {list.data?.inputs.length === 0 && (
        <p className="text-low">Your decision pages will appear here.</p>
      )}
      <ul className="space-y-half">
        {list.data?.inputs.map((detail) => (
          <li key={detail.input.id}>
            <button
              className={`${button} w-full text-left py-base`}
              onClick={() => {
                window.location.hash = `input=${detail.input.id}`;
              }}
            >
              <span className="block font-medium">{detail.version.title}</span>
              <span className="block text-low">
                {detail.input.status === 'open'
                  ? 'Needs answer'
                  : detail.input.status === 'answered'
                    ? 'Needs completion'
                    : detail.input.status}
              </span>
            </button>
          </li>
        ))}
      </ul>
      {list.data?.next_cursor && (
        <button
          className={button}
          onClick={() => setCursor(list.data!.next_cursor!)}
        >
          Next page
        </button>
      )}
      {cursor && (
        <button className={button} onClick={() => setCursor('')}>
          First page
        </button>
      )}
    </section>
  );
}

export function WorkspaceInputViewer({ workspaceId }: { workspaceId: string }) {
  const host = useHostId();
  const [selected, setSelected] = useState(selectedInput);
  useEffect(() => {
    const listener = () => setSelected(selectedInput());
    window.addEventListener('hashchange', listener);
    return () => window.removeEventListener('hashchange', listener);
  }, []);
  if (!selected) return null;
  return (
    <InputViewer
      key={`${host}:${workspaceId}:${selected}`}
      workspaceId={workspaceId}
      inputId={selected}
      onClose={() => {
        const url = new URL(window.location.href);
        url.hash = '';
        window.history.replaceState(null, '', url);
        setSelected(null);
      }}
    />
  );
}

function InputViewer({
  workspaceId,
  inputId,
  onClose,
}: {
  workspaceId: string;
  inputId: string;
  onClose: () => void;
}) {
  const host = useHostId();
  const queryClient = useQueryClient();
  const isMobile = useIsMobile();
  const viewport = useVisualViewportGeometry();
  const [reviewMessage, setReviewMessage] = useState<string | null>(null);
  const detail = useQuery({
    queryKey: ['input', host, workspaceId, inputId],
    queryFn: () => inputsApi.get(workspaceId, host, inputId),
    refetchOnWindowFocus: false,
  });
  return createPortal(
    <Dialog.Root
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/60 z-50" />
        <Dialog.Content
          style={
            isMobile && viewport.height !== null
              ? {
                  height: viewport.height,
                  top: viewport.offsetTop,
                  bottom: 'auto',
                }
              : undefined
          }
          className="new-design fixed inset-0 md:inset-6 z-50 bg-primary text-normal flex flex-col min-h-0 border shadow-xl focus:outline-none"
        >
          <header className="flex items-center gap-base border-b p-base shrink-0">
            <Dialog.Close className={button}>Back</Dialog.Close>
            <button
              className={button}
              disabled={detail.isFetching}
              onClick={async () => {
                const result = await detail.refetch();
                if (!result.error)
                  setReviewMessage(
                    'Review the current version and your saved draft before you submit again.'
                  );
              }}
            >
              Refresh
            </button>
            <Dialog.Title className="text-xl font-medium min-w-0 truncate">
              {detail.data?.version.title ?? 'Input'}
            </Dialog.Title>
          </header>
          <Dialog.Description className="px-base py-half text-low shrink-0">
            The page can propose an answer. Only Submit records your answer.
            Keep secrets outside Inputs.
          </Dialog.Description>
          {reviewMessage && (
            <p role="status" className="px-base py-half">
              {reviewMessage}
            </p>
          )}
          {detail.error && (
            <p role="alert" className="p-base">
              {detail.error.message}
            </p>
          )}
          {detail.isLoading && (
            <p role="status" className="p-base">
              Loading Input…
            </p>
          )}
          {detail.data && (
            <InputBody
              key={detail.data.version.version}
              workspaceId={workspaceId}
              detail={detail.data}
              refreshAfterConflict={async () => {
                const result = await detail.refetch();
                if (result.error) throw result.error;
                setReviewMessage(
                  'The Input changed. Review the current version and your saved draft before you submit again.'
                );
              }}
              refresh={async () => {
                await queryClient.invalidateQueries({
                  queryKey: ['input', host, workspaceId, inputId],
                });
                await queryClient.invalidateQueries({
                  queryKey: ['inputs', host, workspaceId],
                });
              }}
            />
          )}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>,
    document.body
  );
}

function InputBody({
  workspaceId,
  detail,
  refresh,
  refreshAfterConflict,
}: {
  workspaceId: string;
  detail: InputDetail;
  refresh: () => Promise<void>;
  refreshAfterConflict: () => Promise<void>;
}) {
  const host = useHostId();
  const frame = useRef<HTMLIFrameElement>(null);
  const bridge = useRef<InputBridge>(
    createInputBridge(detail.input.id, detail.version.version)
  );
  const key = `bc.input.draft:${host ?? 'local'}:${detail.input.id}:${detail.version.version}`;
  const [draft, setDraft] = useState<InputAnswer>(() => {
    try {
      return (
        JSON.parse(localStorage.getItem(key) ?? 'null') ?? {
          choice_id: null,
          text: null,
        }
      );
    } catch {
      return { choice_id: null, text: null };
    }
  });
  const [proposal, setProposal] = useState(false);
  const [priorDraft, setPriorDraft] = useState<InputAnswer | null>(null);
  const [document, setDocument] = useState('');
  const [pageAttempt, setPageAttempt] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [navigated, setNavigated] = useState(false);
  const [busy, setBusy] = useState(false);
  const [undoSequence, setUndoSequence] = useState<number | null>(null);
  const answerRequest = useRef<AnswerInput | null>(null);
  const statusRequest = useRef<{
    undo: boolean;
    sequence: number;
    id: string;
  } | null>(null);
  useEffect(() => {
    try {
      localStorage.setItem(key, JSON.stringify(draft));
    } catch {
      setError(
        'This browser cannot save the draft. Keep this page open until you submit.'
      );
    }
  }, [key, draft]);
  useEffect(() => {
    let active = true;
    const snapshot = {
      input: { id: detail.input.id },
      version: { version: detail.version.version },
    };
    inputsApi
      .contentUrl(workspaceId, host, snapshot)
      .then((url) => {
        if (active)
          setDocument(
            inputEmbeddingDocument(
              url,
              bridge.current,
              detail.version.content_hash
            )
          );
      })
      .catch((e) => {
        if (active) setError(String(e));
      });
    return () => {
      active = false;
    };
  }, [
    workspaceId,
    host,
    detail.input.id,
    detail.version.version,
    detail.version.content_hash,
    pageAttempt,
  ]);
  useEffect(() => {
    try {
      const prefix = `bc.input.draft:${host ?? 'local'}:${detail.input.id}:`;
      const keys = Object.keys(localStorage).filter(
        (item) => item.startsWith(prefix) && item !== key
      );
      keys.sort(
        (a, b) =>
          Number(b.slice(prefix.length)) - Number(a.slice(prefix.length))
      );
      const saved = keys[0]
        ? JSON.parse(localStorage.getItem(keys[0]) ?? 'null')
        : null;
      if (saved && (saved.choice_id || saved.text)) setPriorDraft(saved);
    } catch {
      /* The current draft remains available when storage is unavailable. */
    }
  }, [host, detail.input.id, key]);
  useEffect(() => {
    const listener = (event: MessageEvent) => {
      if (
        event.source !== frame.current?.contentWindow ||
        event.data?.bridge_id !== bridge.current.id
      )
        return;
      if (event.data?.type === 'bc.input.navigated') {
        bridge.current.valid = false;
        setNavigated(true);
        return;
      }
      const answer = readInputProposal(
        event,
        frame.current?.contentWindow ?? null,
        bridge.current,
        detail.version.contract
      );
      if (answer) {
        setDraft(answer);
        setProposal(true);
        answerRequest.current = null;
      }
    };
    window.addEventListener('message', listener);
    return () => window.removeEventListener('message', listener);
  }, [detail.version.contract]);
  const submit = async () => {
    setBusy(true);
    setError(null);
    const request = answerRequest.current ?? {
      request_id: crypto.randomUUID(),
      expected_sequence: detail.input.sequence,
      version: detail.version.version,
      answer: draft,
    };
    answerRequest.current = request;
    try {
      await inputsApi.answer(workspaceId, host, detail.input.id, request);
      try {
        localStorage.removeItem(key);
      } catch {
        /* The receipt remains authoritative. */
      }
      answerRequest.current = null;
      await refresh();
    } catch (e) {
      try {
        if (await refreshInputAfterConflict(e, refreshAfterConflict))
          answerRequest.current = null;
      } catch (refreshError) {
        setError(String(refreshError));
      }
      setError(
        `${String(e)}. Your draft stays saved. Review the current version before you submit again.`
      );
    } finally {
      setBusy(false);
    }
  };
  const status = async (undo: boolean) => {
    setBusy(true);
    setError(null);
    const sequence = undo ? undoSequence! : detail.input.sequence;
    if (
      statusRequest.current?.undo !== undo ||
      statusRequest.current.sequence !== sequence
    )
      statusRequest.current = { undo, sequence, id: crypto.randomUUID() };
    try {
      const receipt = await inputsApi.status(
        workspaceId,
        host,
        detail.input.id,
        sequence,
        undo,
        statusRequest.current.id
      );
      setUndoSequence(undo ? null : receipt.sequence);
      statusRequest.current = null;
      await refresh();
    } catch (e) {
      try {
        if (await refreshInputAfterConflict(e, refreshAfterConflict))
          statusRequest.current = null;
      } catch (refreshError) {
        setError(String(refreshError));
      }
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };
  const retryPage = () => {
    bridge.current = createInputBridge(detail.input.id, detail.version.version);
    setDocument('');
    setNavigated(false);
    setPageAttempt((attempt) => attempt + 1);
  };
  return (
    <>
      <div className="flex items-center flex-wrap gap-base px-base pb-base shrink-0">
        <span>
          Version {detail.version.version} · {detail.input.status} · From{' '}
          {detail.input.creator}
        </span>
        <button className={button} onClick={retryPage}>
          Retry page
        </button>
        <span className="text-low">
          If the page fails to load, retry to renew its connection.
        </span>
        {['open', 'answered'].includes(detail.input.status) && (
          <button
            className={button}
            disabled={busy}
            onClick={() => status(false)}
          >
            Close
          </button>
        )}
        {undoSequence !== null && (
          <button
            className={button}
            disabled={busy}
            onClick={() => status(true)}
          >
            Undo close
          </button>
        )}
      </div>
      <div className="flex-1 min-h-24 overflow-hidden bg-white">
        {navigated ? (
          <p role="alert" className="p-base text-black">
            Page navigated away. Close and open the Input to load it safely.
          </p>
        ) : (
          document && (
            <iframe
              ref={frame}
              title={`${detail.version.title} sandbox viewer`}
              srcDoc={document}
              className="w-full h-full border-0"
              onLoad={() => {
                if (!inputFrameLoaded(bridge.current)) setNavigated(true);
              }}
            />
          )
        )}
      </div>
      <footer className="sticky bottom-0 bg-primary border-t p-base space-y-half shrink-0 max-h-[50dvh] overflow-y-auto text-xl md:text-base pb-[max(12px,env(safe-area-inset-bottom))]">
        {error && (
          <p role="alert" className="text-error">
            {error}
          </p>
        )}
        {priorDraft && (
          <div className="border rounded p-base">
            <p>
              A saved draft from an earlier version is available. Read this
              version before you use it.
            </p>
            <p className="whitespace-pre-wrap break-words">{priorDraft.text}</p>
            <button
              className={button}
              onClick={() => {
                setDraft({
                  choice_id: detail.version.contract.choices.some(
                    (c) => c.id === priorDraft.choice_id
                  )
                    ? priorDraft.choice_id
                    : null,
                  text: detail.version.contract.allow_text
                    ? priorDraft.text
                    : null,
                });
                setPriorDraft(null);
                answerRequest.current = null;
              }}
            >
              Use saved draft
            </button>
          </div>
        )}
        {detail.input.status === 'open' && (
          <>
            {proposal && (
              <p className="font-medium">
                Proposed by the page. Review before you submit.
              </p>
            )}
            <label className="block">
              Your choice
              <select
                className={`${button} bg-secondary w-full`}
                value={draft.choice_id ?? ''}
                onChange={(e) => {
                  setDraft({ ...draft, choice_id: e.target.value || null });
                  setProposal(false);
                  answerRequest.current = null;
                }}
              >
                <option value="">Select a choice</option>
                {detail.version.contract.choices.map((choice) => (
                  <option key={choice.id} value={choice.id}>
                    {choice.label}
                  </option>
                ))}
              </select>
            </label>
            {detail.version.contract.allow_text && (
              <label className="block">
                Your answer
                <textarea
                  className="border rounded bg-secondary w-full p-base focus-visible:ring-2 focus-visible:ring-brand min-h-16"
                  value={draft.text ?? ''}
                  onChange={(e) => {
                    setDraft({ ...draft, text: e.target.value });
                    setProposal(false);
                    answerRequest.current = null;
                  }}
                />
              </label>
            )}
            {proposal && (
              <p className="whitespace-pre-wrap break-words">
                {detail.version.contract.choices.find(
                  (c) => c.id === draft.choice_id
                )?.label ?? ''}
                {draft.text ? `\n${draft.text}` : ''}
              </p>
            )}
            <button
              className={`${button} bg-brand text-white w-full`}
              disabled={
                busy || !validateInputAnswer(draft, detail.version.contract)
              }
              onClick={submit}
            >
              {busy
                ? 'Saving…'
                : !draft.choice_id &&
                    !draft.text?.trim() &&
                    detail.version.contract.allow_no_answer_yet
                  ? 'Submit: no answer yet'
                  : 'Submit answer'}
            </button>
          </>
        )}
        {detail.input.status === 'answered' && (
          <p>Answer recorded. Close this Input when the work is complete.</p>
        )}
        {['closed', 'cancelled'].includes(detail.input.status) && (
          <p>
            This Input is {detail.input.status}. Your answer stays in the
            ledger.
          </p>
        )}
      </footer>
    </>
  );
}
