import { describe, expect, it, vi } from 'vitest';

import {
  CLI_SEND_MAX_BYTES,
  isComposerSendKey,
  prepareCliText,
  sendComposerText,
} from './cliComposer';

const httpError = (status: number, message = `status ${status}`) =>
  Object.assign(new Error(message), { status });

describe('prepareCliText', () => {
  it('converts CR line endings to newlines and keeps tabs', () => {
    expect(prepareCliText('a\r\nb\rc\td')).toEqual({
      ok: true,
      text: 'a\nb\nc\td',
    });
  });

  it('rejects blank text', () => {
    expect(prepareCliText(' \n\t ')).toEqual({ ok: false, reason: 'empty' });
  });

  it('rejects control characters instead of dropping them', () => {
    for (const ch of ['\u0000', '\u001b', '\u007f', '\u0085', '\u000b']) {
      expect(prepareCliText(`a${ch}b`)).toEqual({
        ok: false,
        reason: 'controlChars',
      });
    }
  });

  it('measures the size cap in UTF-8 bytes', () => {
    const atCap = 'é'.repeat(CLI_SEND_MAX_BYTES / 2);
    expect(prepareCliText(atCap).ok).toBe(true);
    expect(prepareCliText(`${atCap}x`)).toEqual({
      ok: false,
      reason: 'tooLarge',
      bytes: CLI_SEND_MAX_BYTES + 1,
    });
  });
});

describe('isComposerSendKey', () => {
  it('sends on Enter only', () => {
    expect(isComposerSendKey({ key: 'Enter', shiftKey: false })).toBe(true);
    expect(isComposerSendKey({ key: 'Enter', shiftKey: true })).toBe(false);
    expect(isComposerSendKey({ key: 'a', shiftKey: false })).toBe(false);
  });

  it('never sends while an IME composition is open', () => {
    expect(
      isComposerSendKey({ key: 'Enter', shiftKey: false, isComposing: true })
    ).toBe(false);
    expect(
      isComposerSendKey({ key: 'Enter', shiftKey: false, keyCode: 229 })
    ).toBe(false);
  });
});

describe('sendComposerText', () => {
  it('clears the text after a submitted send', async () => {
    const send = vi.fn().mockResolvedValue({ submitted: true });
    expect(await sendComposerText('hi\r\nthere', send)).toEqual({
      clear: true,
      notice: null,
    });
    expect(send).toHaveBeenCalledWith('hi\nthere');
  });

  it('clears but reports text left unsubmitted in the pane', async () => {
    const send = vi.fn().mockResolvedValue({ submitted: false });
    expect(await sendComposerText('hi', send)).toEqual({
      clear: true,
      notice: { kind: 'unsubmitted' },
    });
  });

  it('keeps the text when no agent owns the pane', async () => {
    const send = vi.fn().mockRejectedValue(httpError(409));
    expect(await sendComposerText('hi', send)).toEqual({
      clear: false,
      notice: { kind: 'noAgent' },
    });
  });

  it('keeps the text when tmux does not confirm delivery', async () => {
    const send = vi.fn().mockRejectedValue(httpError(502));
    expect(await sendComposerText('hi', send)).toEqual({
      clear: false,
      notice: { kind: 'notDelivered' },
    });
  });

  it('keeps the text and shows the server reason on other failures', async () => {
    const send = vi
      .fn()
      .mockRejectedValue(httpError(400, 'Text contains control characters'));
    expect(await sendComposerText('hi', send)).toEqual({
      clear: false,
      notice: { kind: 'failed', message: 'Text contains control characters' },
    });
    send.mockRejectedValue(new TypeError('Failed to fetch'));
    expect(await sendComposerText('hi', send)).toEqual({
      clear: false,
      notice: { kind: 'failed', message: 'Failed to fetch' },
    });
  });

  it('does not call the endpoint for text it would reject', async () => {
    const send = vi.fn();
    expect(await sendComposerText('a\u001bb', send)).toEqual({
      clear: false,
      notice: { kind: 'controlChars' },
    });
    expect(await sendComposerText('   ', send)).toEqual({
      clear: false,
      notice: { kind: 'empty' },
    });
    expect(send).not.toHaveBeenCalled();
  });
});
