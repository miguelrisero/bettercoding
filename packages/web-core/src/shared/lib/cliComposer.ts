import type { SendCliTextResponse } from 'shared/types';

/** Mirrors `MAX_CLI_SEND_BYTES` in crates/server/src/routes/terminal.rs. */
export const CLI_SEND_MAX_BYTES = 256 * 1024;

// Unicode Cc, as Rust's `char::is_control`, minus the tab and newline the
// send endpoint accepts.
const DISALLOWED_CONTROL = /[^\P{Cc}\n\t]/u;

export type CliTextCheck =
  | { ok: true; text: string }
  | { ok: false; reason: 'empty' | 'controlChars' }
  | { ok: false; reason: 'tooLarge'; bytes: number };

/**
 * Normalize line endings and check the composer text against the send
 * endpoint's rules, so a rejected text stays in the composer with a reason
 * instead of losing characters.
 */
export function prepareCliText(raw: string): CliTextCheck {
  const text = raw.replace(/\r\n?/g, '\n');
  if (text.trim() === '') return { ok: false, reason: 'empty' };
  if (DISALLOWED_CONTROL.test(text)) {
    return { ok: false, reason: 'controlChars' };
  }
  const bytes = new TextEncoder().encode(text).length;
  if (bytes > CLI_SEND_MAX_BYTES)
    return { ok: false, reason: 'tooLarge', bytes };
  return { ok: true, text };
}

/** Enter sends; Shift+Enter and IME composition keep editing. */
export function isComposerSendKey(event: {
  key: string;
  shiftKey: boolean;
  isComposing?: boolean;
  keyCode?: number;
}): boolean {
  // keyCode 229: Safari reports the Enter that commits an IME candidate this
  // way, with isComposing already false.
  return (
    event.key === 'Enter' &&
    !event.shiftKey &&
    !event.isComposing &&
    event.keyCode !== 229
  );
}

export type ComposerNotice =
  | { kind: 'unsubmitted' }
  | { kind: 'noAgent' }
  | { kind: 'notDelivered' }
  | { kind: 'empty' }
  | { kind: 'controlChars' }
  | { kind: 'tooLarge'; bytes: number }
  | { kind: 'failed'; message: string };

export interface ComposerSendResult {
  /** The text left the composer (it is in the agent's input or submitted). */
  clear: boolean;
  notice: ComposerNotice | null;
}

/**
 * One composer send. The text is cleared only once the pane holds it: a
 * 200 with `submitted: false` means the text sits unsubmitted in the TUI's
 * input, so sending it again would double it.
 */
export async function sendComposerText(
  raw: string,
  send: (text: string) => Promise<SendCliTextResponse>
): Promise<ComposerSendResult> {
  const check = prepareCliText(raw);
  if (!check.ok) {
    return {
      clear: false,
      notice:
        check.reason === 'tooLarge'
          ? { kind: 'tooLarge', bytes: check.bytes }
          : { kind: check.reason },
    };
  }
  try {
    const { submitted } = await send(check.text);
    return {
      clear: true,
      notice: submitted ? null : { kind: 'unsubmitted' },
    };
  } catch (error) {
    // ApiError carries the HTTP status; read it structurally so this module
    // stays free of the API client's imports.
    const status = (error as { status?: number } | null)?.status;
    if (status === 409) return { clear: false, notice: { kind: 'noAgent' } };
    if (status === 502) {
      return { clear: false, notice: { kind: 'notDelivered' } };
    }
    return {
      clear: false,
      notice: {
        kind: 'failed',
        message: error instanceof Error ? error.message : String(error),
      },
    };
  }
}

export interface ComposerState {
  text: string;
  sending: boolean;
  notice: ComposerNotice | null;
}

const IDLE: ComposerState = { text: '', sending: false, notice: null };

/**
 * Composer state per workspace, outside React: a composer unmounted mid-send
 * (hidden, or the workspace switched away and back) remounts onto the same
 * draft, in-flight flag and outcome instead of fresh state, so it can neither
 * send a draft twice nor lose a newer one.
 */
export function createComposerStore() {
  const states = new Map<string, ComposerState>();
  const listeners = new Set<() => void>();

  const get = (workspaceId: string) => states.get(workspaceId) ?? IDLE;
  const update = (workspaceId: string, patch: Partial<ComposerState>) => {
    const next = { ...get(workspaceId), ...patch };
    if (!next.text && !next.sending && !next.notice) states.delete(workspaceId);
    else states.set(workspaceId, next);
    listeners.forEach((listener) => listener());
  };

  return {
    get,
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    /** Edit the draft; an edit dismisses the last send's notice. */
    setText(workspaceId: string, text: string) {
      update(workspaceId, { text, notice: null });
    },
    async submit(
      workspaceId: string,
      send: (text: string) => Promise<SendCliTextResponse>
    ) {
      const { text, sending } = get(workspaceId);
      if (sending || text.trim() === '') return;
      update(workspaceId, { sending: true, notice: null });
      const result = await sendComposerText(text, send);
      update(workspaceId, {
        sending: false,
        notice: result.notice,
        ...(result.clear ? { text: '' } : {}),
      });
    },
  };
}
