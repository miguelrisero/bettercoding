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
