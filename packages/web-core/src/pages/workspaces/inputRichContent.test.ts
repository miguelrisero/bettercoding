import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { describe, expect, it, vi } from 'vitest';
import {
  inputImagePath,
  readInputImage,
  readInputImageRequest,
  inputImageQueue,
} from './inputRichContent';

const origin = 'https://app.test';
const route = '/api/workspaces/w/files/preview/';
const safeRoute = '/api/workspaces/w/files/input-image/';

describe('Input image boundary', () => {
  it('serializes image transport requests and continues after rejection', async () => {
    const enqueue = inputImageQueue();
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const order: number[] = [];
    const first = enqueue(async () => {
      order.push(1);
      await gate;
      throw new Error('unavailable');
    });
    const second = enqueue(async () => {
      order.push(2);
    });
    await Promise.resolve();
    expect(order).toEqual([1]);
    release();
    await Promise.all([first, second]);
    expect(order).toEqual([1, 2]);
  });
  it('requires the current bridge and bounded image index before requesting bytes', () => {
    const bridge = { id: 'b', inputId: 'i', version: 1, valid: true, loads: 1 };
    const data = {
      type: 'bc.input.image.request',
      protocol: 1,
      bridge_id: 'b',
      input_id: 'i',
      version: 1,
      index: 0,
      src: '.bettercoding-uploads/a.png',
    };
    expect(readInputImageRequest(data, bridge)).toEqual({
      index: 0,
      src: '.bettercoding-uploads/a.png',
    });
    for (const invalid of [
      { ...data, bridge_id: 'old' },
      { ...data, version: 2 },
      { ...data, index: -1 },
      { ...data, index: 2000 },
      { ...data, src: {} },
    ])
      expect(readInputImageRequest(invalid, bridge)).toBeNull();
    expect(readInputImageRequest(data, { ...bridge, valid: false })).toBeNull();
  });
  it('accepts current workspace file links and upload paths', () => {
    expect(
      inputImagePath(`${origin}${route}diagram.png`, 'w', null, origin)
    ).toBe(`${safeRoute}diagram.png`);
    expect(
      inputImagePath('.bettercoding-uploads/a b.png', 'w', null, origin)
    ).toBe(`${safeRoute}.bettercoding-uploads/a%20b.png`);
    expect(
      inputImagePath(
        '/api/host/h/workspaces/w/files/preview/a.png',
        'w',
        'h',
        origin
      )
    ).toBe(`${safeRoute}a.png`);
  });
  it.each([
    'https://evil.test/api/workspaces/w/files/preview/a.png',
    '/api/workspaces/other/files/preview/a.png',
    '/api/host/other/workspaces/w/files/preview/a.png',
    '/api/workspaces/w/files/preview/%2e%2e/secret.png',
    '/api/workspaces/w/files/preview/%252e%252e/secret.png',
    '/api/workspaces/w/files/preview/a%2fb.png',
    '/api/workspaces/w/files/preview/a.svg',
    '/api/workspaces/w/files/preview/a.png?redirect=1',
    'https://[invalid',
    'javascript:alert(1)',
    'data:image/svg+xml,<svg onload="alert(1)"/>',
    '//evil.test/a.png',
  ])('rejects unsafe image source %s', (source) => {
    expect(inputImagePath(source, 'w', 'h', origin)).toBeNull();
  });
  it('returns only bounded raster data and rejects redirects or active MIME', async () => {
    expect(
      await readInputImage(
        new Response(new Uint8Array([137, 80, 78, 71]), {
          headers: { 'content-type': 'image/png' },
        })
      )
    ).toBe('data:image/png;base64,iVBORw==');
    expect(
      await readInputImage(
        new Response('<svg/>', {
          headers: { 'content-type': 'image/svg+xml' },
        })
      )
    ).toBeNull();
    expect(
      await readInputImage(
        new Response('x'.repeat(256 * 1024 + 1), {
          headers: { 'content-type': 'image/png' },
        })
      )
    ).toBeNull();
    expect(
      await readInputImage(new Response(null, { status: 302 }))
    ).toBeNull();
  });
});

// Execute the exact trusted script appended by the backend. DOM stand-ins keep
// this suite allocation-free; the browser suite verifies Mermaid SVG layout.
function runtime(blocks: string[], source = '') {
  const listeners: Record<string, (event: unknown) => void> = {};
  const images: {
    src: string;
    getAttribute: (name: string) => string;
    removeAttribute: (name: string) => void;
  }[] = [];
  const nodes = blocks.map((textContent) => ({
    textContent,
    dataset: {},
    innerHTML: '',
    setAttribute() {},
    matches() {
      return !textContent.startsWith('```');
    },
    replaceChildren(value: unknown) {
      this.innerHTML = String(value);
    },
  }));
  const parent = { postMessage: vi.fn() };
  const doc = {
    addEventListener(type: string, fn: (event: unknown) => void) {
      listeners[type] = fn;
    },
    querySelectorAll(selector: string) {
      return selector === 'img' ? images : nodes;
    },
    createElement() {
      return { textContent: '' };
    },
    head: { append() {} },
  };
  const mermaid = {
    initialize() {},
    async render(_id: string, chart: string) {
      if (chart === 'broken') throw new Error('parse error');
      return { svg: '<svg>diagram</svg>' };
    },
  };
  const code = readFileSync(
    new URL(
      '../../../../../crates/server/src/routes/workspaces/input-rich-content.js',
      import.meta.url
    ),
    'utf8'
  );
  runInNewContext(code, {
    window: {
      addEventListener(type: string, fn: (event: unknown) => void) {
        listeners[type] = fn;
      },
    },
    document: doc,
    URL,
    parent,
    mermaid,
  });
  const init = {
    type: 'bc.input.init',
    protocol: 1,
    bridge_id: 'b',
    input_id: 'i',
    version: 1,
    mermaid_source: source,
  };
  return { nodes, listeners, parent, init, images };
}

describe('trusted Input rich content runtime', () => {
  it('opens dynamic nested HTTP links through the native user activation', () => {
    const page = runtime([]);
    for (const type of ['click', 'auxclick']) {
      for (const href of [
        'https://example.com/report',
        'http://example.com/',
      ]) {
        const preventDefault = vi.fn();
        const attributes: Record<string, string> = { href, target: '_top' };
        const anchor = {
          getAttribute: (name: string) => attributes[name],
          setAttribute: (name: string, value: string) => {
            attributes[name] = value;
          },
        };
        page.listeners[type]({
          type,
          button: type === 'auxclick' ? 1 : 0,
          isTrusted: true,
          target: { parentElement: { closest: () => anchor } },
          preventDefault,
        });
        expect(preventDefault).not.toHaveBeenCalled();
        expect(attributes.target).toBe('_blank');
        expect(attributes.rel).toBe('noopener noreferrer');
      }
    }
    expect(page.parent.postMessage).not.toHaveBeenCalled();
  });
  it.each([
    ['area', { href: 'https://example.com/map' }, '_blank'],
    ['area', { href: 'javascript:alert(1)' }, null],
    ['area', { href: '#section' }, '_self'],
    ['a', { 'xlink:href': 'https://example.com/svg' }, '_blank'],
    ['a', { 'xlink:href': 'javascript:alert(1)' }, null],
    ['a', { 'xlink:href': '#section' }, '_self'],
    [
      'a',
      { href: 'https://example.com/primary', 'xlink:href': 'javascript:bad()' },
      '_blank',
    ],
    [
      'a',
      { href: 'javascript:bad()', 'xlink:href': 'https://example.com/fallback' },
      null,
    ],
    ['a', { href: '', 'xlink:href': 'https://example.com/fallback' }, null],
  ] as const)('validates %s link attributes %j', (tag, initial, target) => {
    const page = runtime([]);
    const original: Record<string, string> = initial;
    const attributes: Record<string, string> = { ...original, target: '_top' };
    const preventDefault = vi.fn();
    const anchor = {
      getAttribute: (name: string) => attributes[name] ?? null,
      setAttribute: (name: string, value: string) => {
        attributes[name] = value;
      },
      removeAttribute: (name: string) => {
        delete attributes[name];
      },
    };
    const closest = vi.fn((selector: string) =>
      selector
        .split(', ')
        .some((part) =>
          Object.keys(initial).some(
            (name) => part === `${tag}[${name.replace(':', '\\:')}]`
          )
        )
        ? anchor
        : null
    );
    page.listeners.click({
      type: 'click',
      isTrusted: true,
      target: { closest },
      preventDefault,
    });
    expect(closest).toHaveBeenCalledWith('a[href], area[href], a[xlink\\:href]');
    if (target === null) {
      expect(preventDefault).toHaveBeenCalledOnce();
    } else {
      expect(preventDefault).not.toHaveBeenCalled();
      expect(attributes.target).toBe(target);
      expect(attributes.href).toBe(original.href ?? original['xlink:href']);
      expect(attributes).not.toHaveProperty('xlink:href');
      if (target === '_blank') expect(attributes.rel).toBe('noopener noreferrer');
    }
  });
  it.each([
    'javascript:alert(1)',
    'data:text/html,x',
    'mailto:owner@example.com',
    '/report',
    '//example.com',
    'report.html',
    'https://user:secret@example.com',
    'https://example.com/\\path',
    'https://example.com/\npath',
    'https://example.com/\u007fpath',
    'https://example.com/' + 'x'.repeat(8192),
    '#section\n',
    '',
  ])('blocks the unsupported link destination %s', (href) => {
    const page = runtime([]);
    const preventDefault = vi.fn();
    page.listeners.click({
      type: 'click',
      isTrusted: true,
      target: {
        closest: () => ({
          getAttribute: (name: string) => (name === 'href' ? href : null),
        }),
      },
      preventDefault,
    });
    expect(preventDefault).toHaveBeenCalledOnce();
    expect(page.parent.postMessage).not.toHaveBeenCalled();
  });
  it('keeps fragments in the same frame despite an authored base target', () => {
    const page = runtime([]);
    const attributes: Record<string, string> = {
      href: '#section',
      target: '_blank',
    };
    const preventDefault = vi.fn();
    page.listeners.click({
      type: 'click',
      isTrusted: true,
      target: {
        closest: () => ({
          getAttribute: (name: string) => attributes[name],
          setAttribute: (name: string, value: string) => {
            attributes[name] = value;
          },
          removeAttribute: (name: string) => {
            delete attributes[name];
          },
        }),
      },
      preventDefault,
    });
    expect(preventDefault).not.toHaveBeenCalled();
    expect(attributes.target).toBe('_self');
  });
  it('accepts a bounded link and blocks an oversized normalized URL', () => {
    const page = runtime([]);
    for (const [href, blocked] of [
      ['https://example.com/' + 'x'.repeat(8172), false],
      ['https://example.com/' + 'é'.repeat(2000), true],
    ] as const) {
      const preventDefault = vi.fn();
      const attributes: Record<string, string> = { href };
      page.listeners.click({
        type: 'click',
        isTrusted: true,
        target: {
          closest: () => ({
            getAttribute: (name: string) => attributes[name],
            setAttribute: (name: string, value: string) => {
              attributes[name] = value;
            },
          }),
        },
        preventDefault,
      });
      expect(preventDefault.mock.calls.length).toBe(blocked ? 1 : 0);
      if (!blocked) expect(attributes.target).toBe('_blank');
    }
  });
  it('blocks script-generated link activation', () => {
    const page = runtime([]);
    const preventDefault = vi.fn();
    page.listeners.click({
      type: 'click',
      isTrusted: false,
      target: {
        closest: () => ({
          getAttribute: (name: string) =>
            name === 'href' ? 'https://example.com/' : null,
        }),
      },
      preventDefault,
    });
    expect(preventDefault).toHaveBeenCalledOnce();
    expect(page.parent.postMessage).not.toHaveBeenCalled();
  });
  it('renders HTML and fenced Mermaid blocks while preserving malformed source', async () => {
    const page = runtime([
      'flowchart TD\nA-->B',
      '```mermaid\nsequenceDiagram\nA->>B: Hi\n```',
      'broken',
    ]);
    page.listeners.message({ source: page.parent, data: page.init });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(page.nodes[0].innerHTML).toBe('<svg>diagram</svg>');
    expect(page.nodes[1].innerHTML).toBe('<svg>diagram</svg>');
    expect(page.nodes[2].textContent).toBe('broken');
    expect(page.nodes[2].innerHTML).toBe('');
  });
  it('keeps raster data images and removes active image sources', async () => {
    const page = runtime([]);
    for (const src of [
      'data:image/png;base64,iVBORw==',
      'data:image/svg+xml,<svg onload="alert(1)"/>',
      'javascript:alert(1)',
    ]) {
      page.images.push({
        src,
        getAttribute() {
          return src;
        },
        removeAttribute(name) {
          if (name === 'src') this.src = '';
        },
      });
    }
    page.listeners.message({ source: page.parent, data: page.init });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(page.images.map((image) => image.src)).toEqual([
      'data:image/png;base64,iVBORw==',
      '',
      '',
    ]);
    page.listeners.message({
      source: page.parent,
      data: {
        ...page.init,
        type: 'bc.input.image',
        index: 1,
        src: 'javascript:alert(1)',
      },
    });
    expect(page.images[1].src).toBe('');
  });
  it('ignores initialization from any source other than the embedding parent', async () => {
    const page = runtime(['flowchart TD\nA-->B']);
    page.listeners.message({ source: {}, data: page.init });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(page.nodes[0].innerHTML).toBe('');
  });
});
