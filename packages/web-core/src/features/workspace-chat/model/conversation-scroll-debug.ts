/**
 * Conversation Scroll Diagnostics
 *
 * Opt-in instrumentation for the load-time scroll behaviour. Silent by default
 * (no console output, near-zero overhead — every entry point early-returns).
 * It earned its keep diagnosing a bug the synthetic Playwright harness could
 * not reproduce, so it stays in the tree behind a flag for the next time.
 *
 * Enable from the browser console, then reproduce:
 *   - `window.__VK_SCROLL_DEBUG = true`   → turn it on for this tab, reload.
 *   - `window.__vkScrollReport()`         → grouped by-source write summary.
 *   - `JSON.stringify(window.__vkScrollLog)` → raw ring buffer.
 *
 * Once on, it intercepts EVERY programmatic scroll write on the conversation
 * container (native wheel/touch scrolling doesn't go through these JS setters,
 * so every captured write is code-initiated — exactly the writes that can fight
 * the user). Writes are attributed via `tagScrollWrite()` markers our own code
 * sets; anything untagged is `external` (TanStack's size compensation, the
 * browser, etc.). Output is on visible console levels (`log`/`warn`) because
 * `console.debug` is hidden behind DevTools' "Verbose" filter by default.
 */

interface ScrollWriteRecord {
  t: number;
  kind: 'set' | 'scrollTo' | 'scrollBy' | 'event';
  from: number;
  to: number;
  delta: number;
  source: string;
  msSinceUserInput: number;
}

interface ScrollDebugWindow {
  __VK_SCROLL_DEBUG?: boolean;
  __vkScrollLog?: ScrollWriteRecord[];
  __vkScrollReport?: () => void;
  __vkScrollProbeInstalled?: boolean;
}

const MAX_RECORDS = 5000;
const USER_ACTIVITY_WINDOW_MS = 600;
const FLUSH_INTERVAL_MS = 1000;
const SCROLL_KEYS = new Set([
  'ArrowUp',
  'ArrowDown',
  'PageUp',
  'PageDown',
  'Home',
  'End',
  ' ',
]);

function dbgWindow(): ScrollDebugWindow | null {
  if (typeof window === 'undefined') return null;
  return window as unknown as ScrollDebugWindow;
}

function enabled(w: ScrollDebugWindow): boolean {
  // Opt-in: silent unless explicitly turned on.
  return w.__VK_SCROLL_DEBUG === true;
}

// One-shot attribution for the next intercepted write. Our own code sets this
// immediately before it writes scroll position; the interceptor reads and
// clears it. Untagged writes are therefore external (TanStack/browser).
let pendingWriteTag: string | null = null;

export function tagScrollWrite(tag: string): void {
  pendingWriteTag = tag;
}

let lastUserInputAt = 0;
let lastUserDir: 'up' | 'down' | 'none' = 'none';

function record(w: ScrollDebugWindow, rec: ScrollWriteRecord): void {
  const log = (w.__vkScrollLog ??= []);
  log.push(rec);
  if (log.length > MAX_RECORDS) log.splice(0, log.length - MAX_RECORDS);
}

/**
 * Discrete lifecycle marker (lock acquire/release, follow-bottom suppression).
 * Printed on a visible level so it shows without the Verbose filter.
 */
export function scrollDebug(
  event: string,
  data?: Record<string, unknown>
): void {
  const w = dbgWindow();
  if (!w || !enabled(w)) return;
  record(w, {
    t: Math.round(performance.now()),
    kind: 'event',
    from: 0,
    to: 0,
    delta: 0,
    source: event,
    msSinceUserInput: Math.round(performance.now() - lastUserInputAt),
  });
  // eslint-disable-next-line no-console
  console.log('[vk-scroll]', event, data ?? '');
}

/**
 * Install the per-element scroll-write interceptor. Idempotent across the app
 * lifetime (only the first container is probed; that's the conversation list).
 */
export function installScrollProbe(el: HTMLElement): () => void {
  const w = dbgWindow();
  if (!w || !enabled(w) || w.__vkScrollProbeInstalled) return () => {};
  w.__vkScrollProbeInstalled = true;

  const now = () => Math.round(performance.now());
  const proto = Object.getOwnPropertyDescriptor(Element.prototype, 'scrollTop');
  if (!proto || !proto.get || !proto.set) return () => {};
  const protoGet = proto.get;
  const protoSet = proto.set;

  let pendingFight: ScrollWriteRecord[] = [];
  let flushTimer: number | null = null;

  const scheduleFlush = () => {
    if (flushTimer !== null) return;
    flushTimer = window.setTimeout(() => {
      flushTimer = null;
      if (pendingFight.length === 0) return;
      const bySource: Record<string, number> = {};
      let travel = 0;
      for (const r of pendingFight) {
        bySource[r.source] = (bySource[r.source] ?? 0) + 1;
        travel += Math.abs(r.delta);
      }
      const sources = Object.entries(bySource)
        .map(([s, n]) => `${s}:${n}`)
        .join(' ');
      // eslint-disable-next-line no-console
      console.warn(
        `[vk-scroll] ⚠ FIGHT — ${pendingFight.length} programmatic scrolls ` +
          `moved the view ${Math.round(travel)}px while you were scrolling ` +
          `(${sources}). window.__vkScrollReport() for detail.`
      );
      pendingFight = [];
    }, FLUSH_INTERVAL_MS);
  };

  const capture = (
    kind: ScrollWriteRecord['kind'],
    from: number,
    to: number
  ) => {
    const source = pendingWriteTag ?? 'external';
    pendingWriteTag = null;
    const sinceInput = now() - lastUserInputAt;
    const rec: ScrollWriteRecord = {
      t: now(),
      kind,
      from: Math.round(from),
      to: Math.round(to),
      delta: Math.round(to - from),
      source,
      msSinceUserInput: sinceInput,
    };
    record(w, rec);
    // A programmatic write within the user-activity window is a fight candidate.
    if (sinceInput < USER_ACTIVITY_WINDOW_MS && Math.abs(rec.delta) > 4) {
      pendingFight.push(rec);
      scheduleFlush();
    }
  };

  // Per-element scrollTop accessor that delegates to the prototype.
  Object.defineProperty(el, 'scrollTop', {
    configurable: true,
    get() {
      return protoGet.call(this);
    },
    set(v: number) {
      capture('set', protoGet.call(this), v);
      protoSet.call(this, v);
    },
  });

  const origScrollTo = el.scrollTo.bind(el);
  const origScrollBy = el.scrollBy.bind(el);
  el.scrollTo = function (...args: unknown[]) {
    const from = protoGet.call(el);
    const opt = args[0];
    const to =
      typeof opt === 'object' && opt !== null
        ? ((opt as ScrollToOptions).top ?? from)
        : ((args[1] as number) ?? from);
    capture('scrollTo', from, to);
    return (origScrollTo as (...a: unknown[]) => void)(...args);
  } as typeof el.scrollTo;
  el.scrollBy = function (...args: unknown[]) {
    const from = protoGet.call(el);
    capture('scrollBy', from, from);
    return (origScrollBy as (...a: unknown[]) => void)(...args);
  } as typeof el.scrollBy;

  const onWheel = (e: WheelEvent) => {
    lastUserInputAt = now();
    lastUserDir = e.deltaY < 0 ? 'up' : 'down';
  };
  const onTouch = () => {
    lastUserInputAt = now();
    lastUserDir = 'none';
  };
  const onKey = (e: KeyboardEvent) => {
    if (SCROLL_KEYS.has(e.key)) {
      lastUserInputAt = now();
      lastUserDir =
        e.key === 'ArrowUp' || e.key === 'PageUp' || e.key === 'Home'
          ? 'up'
          : 'down';
    }
  };
  el.addEventListener('wheel', onWheel, { passive: true, capture: true });
  el.addEventListener('touchmove', onTouch, { passive: true, capture: true });
  el.addEventListener('keydown', onKey, { capture: true });

  w.__vkScrollReport = () => {
    const log = w.__vkScrollLog ?? [];
    const bySource: Record<string, { writes: number; px: number }> = {};
    for (const r of log) {
      if (r.kind === 'event') continue;
      const b = (bySource[r.source] ??= { writes: 0, px: 0 });
      b.writes += 1;
      b.px += Math.abs(r.delta);
    }
    const duringActivity = log.filter(
      (r) =>
        r.kind !== 'event' &&
        r.msSinceUserInput < USER_ACTIVITY_WINDOW_MS &&
        Math.abs(r.delta) > 4
    );
    // eslint-disable-next-line no-console
    console.log(
      `[vk-scroll] report: ${log.length} records, ` +
        `${duringActivity.length} programmatic writes during user scrolling`
    );
    // eslint-disable-next-line no-console
    console.table(bySource);
  };

  // Make it obvious the diagnostics are live (visible level).
  // eslint-disable-next-line no-console
  console.log(
    '%c[vk-scroll] diagnostics active — reproduce the jank, then run window.__vkScrollReport()',
    'color:#a60; font-weight:bold'
  );
  void lastUserDir;

  return () => {
    el.removeEventListener('wheel', onWheel, { capture: true } as never);
    el.removeEventListener('touchmove', onTouch, { capture: true } as never);
    el.removeEventListener('keydown', onKey, { capture: true } as never);
    if (flushTimer !== null) window.clearTimeout(flushTimer);
  };
}
