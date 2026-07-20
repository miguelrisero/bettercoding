import { afterEach, describe, expect, it, vi } from 'vitest';

import { blurActiveTerminalTextarea } from './blurActiveTerminalTextarea';

class FakeHTMLElement {
  readonly tagName: string;
  readonly blur = vi.fn();
  readonly classList: { contains: (className: string) => boolean };

  constructor(tagName: string, classNames: string[] = []) {
    this.tagName = tagName;
    const classes = new Set(classNames);
    this.classList = {
      contains: (className) => classes.has(className),
    };
  }
}

function setActiveElement(activeElement: FakeHTMLElement | null) {
  vi.stubGlobal('HTMLElement', FakeHTMLElement);
  vi.stubGlobal('document', { activeElement });
}

describe('blurActiveTerminalTextarea', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('blurs a focused xterm helper textarea', () => {
    const textarea = new FakeHTMLElement('TEXTAREA', ['xterm-helper-textarea']);
    setActiveElement(textarea);

    expect(blurActiveTerminalTextarea()).toBe(true);
    expect(textarea.blur).toHaveBeenCalledOnce();
  });

  it('leaves another focused element untouched', () => {
    const input = new FakeHTMLElement('INPUT');
    setActiveElement(input);

    expect(blurActiveTerminalTextarea()).toBe(false);
    expect(input.blur).not.toHaveBeenCalled();
  });

  it('returns false when there is no active element', () => {
    setActiveElement(null);

    expect(blurActiveTerminalTextarea()).toBe(false);
  });
});
