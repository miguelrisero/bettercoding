/**
 * Conversation Scroll Diagnostics
 *
 * Lightweight, opt-out instrumentation for the load-time scroll behaviour.
 * Shipped enabled so a real-world reproduction of the "random scrolls while a
 * chat loads" issue records what actually happened — the synthetic Playwright
 * harness could not trigger it, so we capture it from the real session instead.
 *
 * Usage in the browser console:
 *   - `window.__VK_SCROLL_DEBUG = false`  → silence it for this tab.
 *   - `JSON.stringify(window.__vkScrollLog)` → copy the full event ring buffer.
 *   - `window.__vkScrollSummary()` → print a one-line health summary.
 *
 * Console output is deliberately sparse: lock acquire/release and follow-bottom
 * decisions print individually; `repin` events (which can storm) are coalesced,
 * and a repin that fires *while the user is actively scrolling away from the
 * bottom* is flagged as `FIGHTING` — that is the signature of the bug, and with
 * the fix in place it should not appear.
 */

interface ScrollDebugRecord {
  t: number;
  event: string;
  [key: string]: unknown;
}

interface ScrollDebugWindow {
  __VK_SCROLL_DEBUG?: boolean;
  __vkScrollLog?: ScrollDebugRecord[];
  __vkScrollSummary?: () => void;
}

const MAX_RECORDS = 4000;

function debugWindow(): ScrollDebugWindow | null {
  if (typeof window === 'undefined') return null;
  return window as unknown as ScrollDebugWindow;
}

function debugEnabled(w: ScrollDebugWindow): boolean {
  // Default ON in this build; set window.__VK_SCROLL_DEBUG = false to disable.
  return w.__VK_SCROLL_DEBUG !== false;
}

// Coalesce repin bursts so a storm doesn't flood the console.
let repinRun = 0;
let fightingRun = 0;
let repinRunStart = 0;

function flushRepinRun(now: number): void {
  if (repinRun === 0) return;
  const span = now - repinRunStart;
  if (fightingRun > 0) {
    // eslint-disable-next-line no-console
    console.warn(
      `[vk-scroll] FIGHTING repin x${fightingRun}/${repinRun} over ${span}ms ` +
        `— bottom-lock re-pinned against active user scroll (this is the bug)`
    );
  } else {
    // eslint-disable-next-line no-console
    console.debug(
      `[vk-scroll] repin x${repinRun} over ${span}ms (following bottom, expected)`
    );
  }
  repinRun = 0;
  fightingRun = 0;
}

export function scrollDebug(
  event: string,
  data?: Record<string, unknown>
): void {
  const w = debugWindow();
  if (!w || !debugEnabled(w)) return;

  const now = Math.round(performance.now());
  const record: ScrollDebugRecord = { t: now, event, ...data };

  const log = (w.__vkScrollLog ??= []);
  log.push(record);
  if (log.length > MAX_RECORDS) log.splice(0, log.length - MAX_RECORDS);

  if (!w.__vkScrollSummary) {
    w.__vkScrollSummary = () => {
      const entries = w.__vkScrollLog ?? [];
      const counts: Record<string, number> = {};
      for (const e of entries) counts[e.event] = (counts[e.event] ?? 0) + 1;
      // eslint-disable-next-line no-console
      console.table(counts);
    };
  }

  if (event === 'repin') {
    if (repinRun === 0) repinRunStart = now;
    repinRun += 1;
    if (data?.fighting) fightingRun += 1;
    // Flush periodically so a long storm still surfaces without spamming.
    if (repinRun >= 60) flushRepinRun(now);
    return;
  }

  // A non-repin event ends any in-progress repin run; surface it first.
  flushRepinRun(now);
  // eslint-disable-next-line no-console
  console.debug('[vk-scroll]', event, data ?? '');
}
