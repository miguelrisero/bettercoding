import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import * as Dialog from '@radix-ui/react-dialog';
import {
  ArrowClockwiseIcon,
  CaretLeftIcon,
  CaretRightIcon,
  CheckIcon,
  XIcon,
} from '@phosphor-icons/react';
import type { AnswerInput, InputAnswer, InputDetail } from 'shared/types';
import { useUiPreferencesStore } from '@/shared/stores/useUiPreferencesStore';
import { useHostId } from '@/shared/providers/HostIdProvider';
import { useVisualViewportGeometry } from '@/shared/hooks/useVisualViewportGeometry';
import { useIsMobile } from '@/shared/hooks/useIsMobile';
import { cn } from '@/shared/lib/utils';
import { formatDateShortWithTime, formatRelativeTime } from '@/shared/lib/date';
import {
  inputEmbeddingDocument,
  inputFrameLoaded,
  createInputBridge,
  readInputProposal,
  validateInputAnswer,
  type InputBridge,
} from './inputBridge';
import {
  inputsApi,
  loadPendingInputs,
  refreshInputAfterConflict,
} from './inputsApi';
import {
  compareInputs,
  inputStatusLabel,
  nextOpenInput,
  splitRecommended,
} from './inputFlow';

const focusRing =
  'focus:outline-none focus-visible:ring-2 focus-visible:ring-brand';
const iconButton = `inline-flex items-center justify-center size-9 shrink-0 rounded-md text-low hover:text-high hover:bg-secondary disabled:opacity-40 disabled:pointer-events-none ${focusRing}`;
const quietButton = `text-sm text-low hover:text-high underline-offset-2 hover:underline disabled:opacity-50 ${focusRing}`;

function selectedInput(): string | null {
  const value = new URLSearchParams(window.location.hash.slice(1)).get('input');
  return value && /^[0-9a-f-]{36}$/i.test(value) ? value : null;
}

function clearInputHash() {
  const url = new URL(window.location.href);
  url.hash = '';
  window.history.replaceState(null, '', url);
}

function StatusDot({ status }: { status: string }) {
  return (
    <span
      aria-hidden
      className={cn(
        'size-1.5 rounded-full shrink-0',
        status === 'open'
          ? 'bg-brand'
          : status === 'answered'
            ? 'bg-success'
            : 'bg-low'
      )}
    />
  );
}

function PriorityBadge({ priority }: { priority: string }) {
  return (
    <span
      className={cn(
        'rounded border px-1.5 py-0.5 text-xs font-semibold',
        priority === 'P0'
          ? 'text-error bg-error/10'
          : priority === 'P1'
            ? 'text-brand bg-brand/10'
            : 'text-low'
      )}
    >
      {priority}
    </span>
  );
}

function InputSortControl() {
  const sort = useUiPreferencesStore((s) => s.inputSort);
  const setSort = useUiPreferencesStore((s) => s.setInputSort);
  return (
    <label className="flex items-center gap-half text-sm text-low">
      Sort
      <select
        className={`min-h-9 rounded border bg-primary px-half text-high ${focusRing}`}
        value={sort}
        onChange={(e) =>
          setSort(e.target.value === 'newest' ? 'newest' : 'priority')
        }
      >
        <option value="priority">Priority</option>
        <option value="newest">Newest</option>
      </select>
    </label>
  );
}

function Age({ date }: { date: string }) {
  return (
    <time dateTime={date} title={formatDateShortWithTime(date)}>
      {formatRelativeTime(date)}
    </time>
  );
}

export function WorkspaceInputsContainer({
  workspaceId,
}: {
  workspaceId: string;
}) {
  const host = useHostId();
  const sort = useUiPreferencesStore((s) => s.inputSort);
  const [showDone, setShowDone] = useState(false);
  const [cursor, setCursor] = useState('');
  const list = useQuery({
    queryKey: ['inputs', host, workspaceId, showDone, cursor],
    queryFn: () =>
      showDone
        ? inputsApi.list(workspaceId, host, true, cursor)
        : loadPendingInputs(workspaceId, host),
    refetchInterval: 10000,
  });
  return (
    <section className="w-full p-base space-y-base" aria-label="Inputs">
      <div className="flex items-center justify-between gap-base">
        <span className="text-sm text-low">
          <strong className="font-medium text-high tabular-nums">
            {list.data?.pending_count ?? 0}
          </strong>{' '}
          pending
        </span>
        <div className="flex gap-base" role="group" aria-label="Input view">
          {[false, true].map((isDone) => (
            <button
              key={String(isDone)}
              className={cn(
                quietButton,
                showDone === isDone && 'text-high underline'
              )}
              aria-pressed={showDone === isDone}
              onClick={() => {
                setShowDone(isDone);
                setCursor('');
              }}
            >
              {isDone ? 'Done' : 'Pending'}
            </button>
          ))}
        </div>
      </div>
      <InputSortControl />
      {list.isLoading && (
        <p role="status" className="text-sm text-low">
          Loading Inputs…
        </p>
      )}
      {list.error && (
        <p role="alert" className="text-sm text-error">
          {list.error.message}
        </p>
      )}
      {list.data?.inputs.length === 0 && (
        <p className="text-sm text-low">
          Your decision pages will appear here.
        </p>
      )}
      <ul className="-mx-half">
        {(list.data
          ? [...list.data.inputs].sort((a, b) => compareInputs(a, b, sort))
          : []
        ).map((detail) => (
          <li key={detail.input.id}>
            <button
              className={`w-full text-left rounded-md px-half py-base hover:bg-secondary ${focusRing}`}
              onClick={() => {
                window.location.hash = `input=${detail.input.id}`;
              }}
            >
              <span className="block text-sm text-high leading-snug line-clamp-2">
                {detail.version.title}
              </span>
              <span className="mt-1 flex items-center gap-half text-xs text-low [&>time]:ml-half">
                <PriorityBadge priority={detail.input.priority} />
                <StatusDot status={detail.input.status} />
                {inputStatusLabel[detail.input.status] ?? detail.input.status}
                <Age date={detail.input.created_at} />
              </span>
            </button>
          </li>
        ))}
      </ul>
      {(list.data?.next_cursor || cursor) && (
        <div className="flex gap-base">
          {cursor && (
            <button className={quietButton} onClick={() => setCursor('')}>
              First page
            </button>
          )}
          {list.data?.next_cursor && (
            <button
              className={quietButton}
              onClick={() => setCursor(list.data!.next_cursor!)}
            >
              Next page
            </button>
          )}
        </div>
      )}
    </section>
  );
}

export function WorkspaceInputViewer({ workspaceId }: { workspaceId: string }) {
  const host = useHostId();
  const queryClient = useQueryClient();
  const sort = useUiPreferencesStore((s) => s.inputSort);
  const isMobile = useIsMobile();
  const viewport = useVisualViewportGeometry();
  const [selected, setSelected] = useState(selectedInput);
  const [done, setDone] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const content = useRef<HTMLDivElement>(null);
  // Bumped whenever the owner closes or moves, so a late answer cannot advance.
  const session = useRef(0);
  useEffect(() => {
    const listener = () => {
      session.current += 1;
      const next = selectedInput();
      setSelected(next);
      if (next) setDone(false);
    };
    window.addEventListener('hashchange', listener);
    return () => window.removeEventListener('hashchange', listener);
  }, []);
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), 3000);
    return () => clearTimeout(timer);
  }, [notice]);
  // Shares the cache entry of the pending list in the Inputs panel.
  const listQuery = {
    queryKey: ['inputs', host, workspaceId, false, ''],
    queryFn: () => loadPendingInputs(workspaceId, host),
  };
  const list = useQuery({ ...listQuery, enabled: selected !== null });
  if (!selected && !done) return null;
  const open = (id: string) => {
    window.location.hash = `input=${id}`;
  };
  const close = () => {
    session.current += 1;
    clearInputHash();
    setSelected(null);
    setDone(false);
  };
  const advance = async (from: InputDetail) => {
    const fromId = from.input.id;
    const started = session.current;
    let inputs: InputDetail[];
    try {
      const page = await queryClient.fetchQuery({ ...listQuery, staleTime: 0 });
      inputs = page.inputs;
    } catch {
      return; // Stay on the answered Input; its receipt is already shown.
    }
    if (session.current !== started || selectedInput() !== fromId) return;
    const next = nextOpenInput(
      inputs,
      queryClient.getQueryData<InputDetail>([
        'input',
        host,
        workspaceId,
        fromId,
      ]) ?? from,
      useUiPreferencesStore.getState().inputSort
    );
    const stillOpen = inputs.some((d) => d.input.id === fromId);
    if (!next && stillOpen) {
      setNotice('Saved');
      return; // "No answer yet" keeps the only open Input in view.
    }
    setNotice('Answer saved');
    if (next) {
      open(next);
    } else {
      clearInputHash();
      setSelected(null);
      setDone(true);
    }
  };
  const openInputs = [...(list.data?.inputs ?? [])].sort((a, b) =>
    compareInputs(a, b, sort)
  );
  const position = openInputs.findIndex((d) => d.input.id === selected);
  return createPortal(
    <Dialog.Root
      open
      onOpenChange={(isOpen) => {
        if (!isOpen) close();
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/60 z-50" />
        <Dialog.Content
          ref={content}
          // Focus the dialog itself so the close button shows no ring on open.
          onOpenAutoFocus={(event) => {
            event.preventDefault();
            content.current?.focus();
          }}
          style={
            isMobile && viewport.height !== null
              ? {
                  height: viewport.height,
                  top: viewport.offsetTop,
                  bottom: 'auto',
                }
              : undefined
          }
          className="new-design fixed inset-0 md:inset-6 z-50 bg-primary text-normal flex flex-col min-h-0 md:rounded-lg md:border shadow-xl overflow-hidden focus:outline-none"
        >
          <p role="status" className="sr-only">
            {notice}
          </p>
          {selected ? (
            <InputViewer
              key={`${host}:${workspaceId}:${selected}`}
              workspaceId={workspaceId}
              inputId={selected}
              notice={notice}
              position={
                position < 0
                  ? null
                  : {
                      index: position,
                      total: openInputs.length,
                      step: (delta: number) =>
                        open(
                          openInputs[
                            (position + delta + openInputs.length) %
                              openInputs.length
                          ].input.id
                        ),
                    }
              }
              onAnswered={advance}
            />
          ) : (
            <InputsDone />
          )}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>,
    document.body
  );
}

const BURST = Array.from({ length: 12 }, (_, i) => i * 30);

function InputsDone() {
  return (
    <div className="flex-1 grid place-items-center p-double">
      <div className="text-center max-w-sm">
        <div className="relative mx-auto mb-double size-20">
          {BURST.map((angle, i) => (
            <span
              key={angle}
              aria-hidden
              style={{ ['--angle' as string]: `${angle}deg` }}
              className={cn(
                'input-done-spark absolute left-1/2 top-1/2 size-2 -ml-1 -mt-1 rounded-full',
                i % 2 ? 'bg-success' : 'bg-brand'
              )}
            />
          ))}
          <div className="input-done-check absolute inset-0 grid place-items-center rounded-full bg-success text-white">
            <CheckIcon className="size-9" weight="bold" />
          </div>
        </div>
        <Dialog.Title className="text-xl font-semibold text-high">
          All caught up
        </Dialog.Title>
        <Dialog.Description className="mt-half text-low">
          You answered every open Input. New ones show up in the Inputs panel.
        </Dialog.Description>
        <Dialog.Close
          className={`mt-double min-h-11 px-double rounded-md border text-high hover:bg-secondary ${focusRing}`}
        >
          Done
        </Dialog.Close>
      </div>
    </div>
  );
}

function InputViewer({
  workspaceId,
  inputId,
  notice,
  position,
  onAnswered,
}: {
  workspaceId: string;
  inputId: string;
  notice: string | null;
  position: {
    index: number;
    total: number;
    step: (delta: number) => void;
  } | null;
  onAnswered: (detail: InputDetail) => Promise<void>;
}) {
  const host = useHostId();
  const queryClient = useQueryClient();
  const [priorityBusy, setPriorityBusy] = useState(false);
  const [priorityError, setPriorityError] = useState<string | null>(null);
  const [reviewMessage, setReviewMessage] = useState<string | null>(null);
  const detail = useQuery({
    queryKey: ['input', host, workspaceId, inputId],
    queryFn: () => inputsApi.get(workspaceId, host, inputId),
    refetchOnWindowFocus: false,
  });
  const input = detail.data?.input;
  return (
    <>
      <header className="flex items-start gap-base px-base md:px-double pt-base md:pt-double pb-base border-b shrink-0">
        <Dialog.Close aria-label="Close" className={cn(iconButton, '-ml-1')}>
          <XIcon className="size-5" />
        </Dialog.Close>
        <div className="min-w-0 flex-1 pt-1">
          <Dialog.Title
            className="text-xl font-semibold text-high leading-snug line-clamp-2 [text-wrap:balance]"
            title={detail.data?.version.title}
          >
            {detail.data?.version.title ?? 'Input'}
          </Dialog.Title>
          {input && (
            <div className="mt-1 flex flex-wrap items-center gap-x-base gap-y-half text-sm text-low">
              <span className="inline-flex items-center gap-half text-normal">
                <StatusDot status={input.status} />
                {inputStatusLabel[input.status] ?? input.status}
              </span>
              <PriorityBadge priority={input.priority} />
              {['open', 'answered'].includes(input.status) && (
                <label className="inline-flex items-center gap-half">
                  Priority
                  <select
                    aria-label="Input priority"
                    className={`min-h-9 rounded border bg-primary px-half text-high ${focusRing}`}
                    value={input.priority}
                    disabled={priorityBusy}
                    onChange={async (e) => {
                      const priority = e.target.value;
                      setPriorityBusy(true);
                      setPriorityError(null);
                      try {
                        const result = await inputsApi.priority(
                          workspaceId,
                          host,
                          inputId,
                          priority
                        );
                        queryClient.setQueryData<InputDetail>(
                          ['input', host, workspaceId, inputId],
                          (data) => (data ? { ...data, input: result } : data)
                        );
                        await queryClient.invalidateQueries({
                          queryKey: ['inputs', host, workspaceId],
                        });
                      } catch (error) {
                        setPriorityError(
                          error instanceof Error
                            ? error.message
                            : 'Could not save priority'
                        );
                      } finally {
                        setPriorityBusy(false);
                      }
                    }}
                  >
                    {['P0', 'P1', 'P2', 'P3', 'P4'].map((priority) => (
                      <option key={priority} value={priority}>
                        {priority}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              <Age date={input.created_at} />
              {position && (
                <span className="md:hidden tabular-nums">
                  {position.index + 1} of {position.total}
                </span>
              )}
              {input.current_version > 1 && (
                <span>Version {detail.data?.version.version}</span>
              )}
              {notice && (
                <span className="inline-flex items-center gap-1 text-success">
                  <CheckIcon className="size-3.5" weight="bold" />
                  {notice}
                </span>
              )}
            </div>
          )}
        </div>
        <div className="flex items-center shrink-0">
          {position && (
            <nav
              aria-label="Open Inputs"
              className="hidden md:flex items-center"
            >
              {position.total > 1 && (
                <button
                  aria-label="Previous open Input"
                  className={iconButton}
                  onClick={() => position.step(-1)}
                >
                  <CaretLeftIcon className="size-4" />
                </button>
              )}
              <span className="px-half text-sm text-low tabular-nums whitespace-nowrap">
                {position.index + 1} of {position.total}
              </span>
              {position.total > 1 && (
                <button
                  aria-label="Next open Input"
                  className={iconButton}
                  onClick={() => position.step(1)}
                >
                  <CaretRightIcon className="size-4" />
                </button>
              )}
            </nav>
          )}
          <button
            aria-label="Refresh Input"
            title="Refresh Input"
            className={iconButton}
            disabled={detail.isFetching}
            onClick={async () => {
              const result = await detail.refetch();
              if (!result.error)
                setReviewMessage(
                  'Review the current version and your saved draft before you submit again.'
                );
            }}
          >
            <ArrowClockwiseIcon
              className={cn('size-4', detail.isFetching && 'animate-spin')}
            />
          </button>
        </div>
      </header>
      <Dialog.Description className="sr-only">
        The page can propose an answer. Only Submit records your answer. Keep
        secrets outside Inputs.
      </Dialog.Description>
      {priorityError && (
        <p role="alert" className="px-base py-half text-error">
          {priorityError}
        </p>
      )}
      {reviewMessage && (
        <p
          role="status"
          className="px-base md:px-double py-half text-sm bg-secondary border-b shrink-0"
        >
          {reviewMessage}
        </p>
      )}
      {detail.error && (
        <p role="alert" className="p-double text-error">
          {detail.error.message}
        </p>
      )}
      {detail.isLoading && (
        <p role="status" className="p-double text-low">
          Loading Input…
        </p>
      )}
      {detail.data && (
        <InputBody
          key={detail.data.version.version}
          workspaceId={workspaceId}
          detail={detail.data}
          onAnswered={onAnswered}
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
    </>
  );
}

function InputBody({
  workspaceId,
  detail,
  refresh,
  refreshAfterConflict,
  onAnswered,
}: {
  workspaceId: string;
  detail: InputDetail;
  refresh: () => Promise<void>;
  refreshAfterConflict: () => Promise<void>;
  onAnswered: (detail: InputDetail) => Promise<void>;
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
  // A closed or moved dialog unmounts this body; its late answer must not advance.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
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
    let answered = false;
    try {
      await inputsApi.answer(workspaceId, host, detail.input.id, request);
      try {
        localStorage.removeItem(key);
      } catch {
        /* The receipt remains authoritative. */
      }
      answerRequest.current = null;
      await refresh();
      answered = true;
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
    if (answered && mounted.current) await onAnswered(detail);
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
  const canSubmit =
    !busy && validateInputAnswer(draft, detail.version.contract);
  const choices = detail.version.contract.choices;
  return (
    <div className="flex-1 min-h-0 flex flex-col md:flex-row">
      <div className="relative flex-1 min-h-24 bg-white">
        {navigated ? (
          <p role="alert" className="p-double text-black">
            Page navigated away. Close and open the Input to load it safely.
          </p>
        ) : (
          document && (
            <iframe
              ref={frame}
              title={`${detail.version.title} sandbox viewer`}
              srcDoc={document}
              className="absolute inset-0 w-full h-full border-0"
              onLoad={() => {
                if (!inputFrameLoaded(bridge.current)) setNavigated(true);
              }}
            />
          )
        )}
      </div>
      <aside
        aria-label="Your answer"
        className="md:w-96 shrink-0 flex flex-col max-h-[55dvh] md:max-h-none overflow-y-auto border-t md:border-t-0 md:border-l bg-primary"
        onKeyDown={(e) => {
          if (
            e.key === 'Enter' &&
            (e.metaKey || e.ctrlKey) &&
            detail.input.status === 'open' &&
            canSubmit
          ) {
            e.preventDefault();
            void submit();
          }
        }}
      >
        <div className="flex-1 p-base md:p-double space-y-double">
          {priorDraft && (
            <div className="rounded-md border p-base space-y-half text-sm">
              <p className="text-high">
                A saved draft from an earlier version is available. Read this
                version before you use it.
              </p>
              {priorDraft.text && (
                <p className="whitespace-pre-wrap break-words text-low">
                  {priorDraft.text}
                </p>
              )}
              <button
                className={quietButton}
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
                <div className="rounded-md border border-brand p-base space-y-half text-sm">
                  <p className="font-medium text-high">
                    Proposed by the page. Review before you submit.
                  </p>
                  <p className="whitespace-pre-wrap break-words">
                    {detail.version.contract.choices.find(
                      (c) => c.id === draft.choice_id
                    )?.label ?? ''}
                    {draft.text ? `\n${draft.text}` : ''}
                  </p>
                </div>
              )}
              {choices.length > 0 && (
                <fieldset className="space-y-half">
                  <legend className="mb-base text-sm font-medium text-high">
                    Your choice
                  </legend>
                  {choices.map((choice) => {
                    const { label, recommended } = splitRecommended(
                      choice.label
                    );
                    const checked = draft.choice_id === choice.id;
                    return (
                      <label
                        key={choice.id}
                        className={cn(
                          'flex items-start gap-base rounded-md border p-base cursor-pointer transition-colors has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-brand',
                          checked
                            ? 'border-brand bg-secondary'
                            : 'hover:bg-secondary'
                        )}
                      >
                        <input
                          type="radio"
                          name={`input-choice-${detail.input.id}`}
                          value={choice.id}
                          checked={checked}
                          className="mt-1 size-4 shrink-0 accent-brand focus:outline-none"
                          onChange={() => {
                            setDraft({ ...draft, choice_id: choice.id });
                            setProposal(false);
                            answerRequest.current = null;
                          }}
                        />
                        <span className="min-w-0 flex-1 text-high leading-snug">
                          {label}
                          {recommended && (
                            <span className="mt-1 flex w-fit items-center gap-1 rounded-full bg-brand px-2 py-px text-xs font-medium text-on-brand">
                              Recommended
                            </span>
                          )}
                        </span>
                      </label>
                    );
                  })}
                  {draft.choice_id && (
                    <button
                      className={quietButton}
                      onClick={() => {
                        setDraft({ ...draft, choice_id: null });
                        setProposal(false);
                        answerRequest.current = null;
                      }}
                    >
                      Clear choice
                    </button>
                  )}
                </fieldset>
              )}
              {detail.version.contract.allow_text && (
                <label className="block space-y-half">
                  <span className="block text-sm font-medium text-high">
                    {choices.length > 0 ? 'Add a note' : 'Your answer'}
                  </span>
                  <textarea
                    className={`block w-full min-h-28 rounded-md border bg-secondary p-base text-base text-high placeholder:text-low resize-y ${focusRing}`}
                    placeholder={
                      choices.length > 0
                        ? 'Optional context for the agent'
                        : 'Write your answer'
                    }
                    value={draft.text ?? ''}
                    onChange={(e) => {
                      setDraft({ ...draft, text: e.target.value });
                      setProposal(false);
                      answerRequest.current = null;
                    }}
                  />
                </label>
              )}
            </>
          )}
          {detail.input.status === 'answered' && (
            <p className="text-high">
              Answer recorded. Close this Input when the work is complete.
            </p>
          )}
          {['closed', 'cancelled'].includes(detail.input.status) && (
            <p className="text-high">
              This Input is {detail.input.status}. Your answer stays in the
              ledger.
            </p>
          )}
        </div>
        <div className="sticky bottom-0 bg-primary border-t p-base md:px-double space-y-base pb-[max(12px,env(safe-area-inset-bottom))]">
          {error && (
            <p role="alert" className="text-sm text-error break-words">
              {error}
            </p>
          )}
          {detail.input.status === 'open' && (
            <button
              className={`w-full min-h-11 rounded-md bg-brand hover:bg-brand-hover text-on-brand font-medium disabled:opacity-50 ${focusRing} focus-visible:ring-offset-2`}
              disabled={!canSubmit}
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
          )}
          <p className="text-xs text-low">
            The page can propose an answer. Only Submit records your answer.
            Keep secrets outside Inputs.
          </p>
          <div className="flex flex-wrap gap-x-double gap-y-half">
            <button
              className={quietButton}
              title="If the page fails to load, reload it to renew its connection."
              onClick={retryPage}
            >
              Reload page
            </button>
            {['open', 'answered'].includes(detail.input.status) && (
              <button
                className={quietButton}
                disabled={busy}
                onClick={() => status(false)}
              >
                Close Input
              </button>
            )}
            {undoSequence !== null && (
              <button
                className={quietButton}
                disabled={busy}
                onClick={() => status(true)}
              >
                Undo close
              </button>
            )}
          </div>
        </div>
      </aside>
    </div>
  );
}
