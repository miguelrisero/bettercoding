// Loaded by the isolated Input browser job. All records and responses are synthetic.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const root = '/fixture/queue-dist';
const workspace = '00000000-0000-4000-8000-000000000099';
const api = `/api/workspaces/${workspace}/inputs`;
const ids = [1, 2, 3].map((n) => `00000000-0000-4000-8000-00000000000${n}`);
const titles = ['First card', 'Informational card', 'Last card'];
const timestamp = '2026-11-01T00:00:00Z';
let cards;
let writes;
let closeRequests = [];
let unexpectedWrite;
let rejectClose;
let failDoneAfterClose;
let doneFailures;

function reset(only) {
  writes = [];
  closeRequests = [];
  unexpectedWrite = false;
  rejectClose = false;
  failDoneAfterClose = false;
  doneFailures = 0;
  cards = ids.map((id, i) => ({
    input: {
      id, workspace_id: workspace, external_key: `fixture-${i}`, creator: 'fixture',
      show_at: null, expires_at: null, priority: 'P2', current_version: 1,
      status: only !== undefined && only !== i ? 'closed' : 'open',
      sequence: 1, created_at: timestamp, updated_at: timestamp,
    },
    version: {
      version: 1, title: titles[i], html: null, content_hash: `synthetic-${i}`,
      contract: {
        choices: i === 1 ? [] : [{ id: 'A', label: 'Choice A' }],
        allow_text: i !== 1, allow_no_answer_yet: i === 1,
      },
    },
    answers: [],
  }));
}

export function matches(path) {
  return path === '/queue' || path.startsWith('/assets/') || path === api || path.startsWith(`${api}/`);
}

export async function respond(req, res, path, csp) {
  const send = (body, type = 'application/json') => {
    res.setHeader('Content-Type', type);
    return res.end(body);
  };
  const data = (value) => send(JSON.stringify({ success: true, data: value }));
  const deny = () => {
    res.statusCode = 404;
    return send(JSON.stringify({ success: false, message: 'Fixture route unavailable' }));
  };
  const [id, suffix, version, content] = path.slice(api.length + 1).split('/');
  const card = cards?.find((detail) => detail.input.id === id);
  if (req.method === 'POST' && path === `${api}/${id}/close` && closeRequests.length < 4) {
    const request = { id: id.slice(0, 36), responseStatus: null };
    closeRequests.push(request);
    res.once('finish', () => { request.responseStatus = res.statusCode; });
  }
  if (req.method === 'POST' && card && suffix === 'close' && path === `${api}/${id}/close`) {
    let body = '';
    for await (const chunk of req) {
      body += chunk;
      assert.ok(body.length <= 4096, 'Bound the synthetic request body');
    }
    const request = JSON.parse(body);
    const { expected_sequence, request_id, status, reason } = request;
    assert.equal(status, 'closed');
    assert.equal(card.input.status, 'open');
    assert.equal(expected_sequence, card.input.sequence);
    assert.ok(request_id);
    assert.ok(reason === null || reason === 'marked_as_seen');
    if (rejectClose) {
      res.statusCode = 503;
      return send(JSON.stringify({ success: false, message: 'Synthetic close failed' }));
    }
    card.input.status = 'closed';
    card.input.sequence++;
    const sequence = card.input.sequence;
    writes.push({ id, reason });
    return data({
      input_id: id, version: 1, sequence, status,
      owner_url: `/queue#input=${id}`,
      event: {
        id: request_id, cursor: sequence, input_id: id, workspace_id: workspace,
        sequence, version: 1, event_type: 'closed', actor_kind: 'owner_ui',
        actor: 'owner', payload: { prior_status: 'open', reason, status, show_at: null },
        request_id, request_hash: 'synthetic', hash: 'synthetic', prev_hash: 'synthetic',
        created_at: timestamp,
      },
    });
  }
  if (req.method !== 'GET') {
    unexpectedWrite = true;
    return deny();
  }
  if (path === '/queue') {
    res.setHeader('Content-Security-Policy', "connect-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self' data:");
    return send(readFileSync(`${root}/test-fixtures/input-queue/index.html`), 'text/html; charset=utf-8');
  }
  if (/^\/assets\/[\w.-]+\.(css|js)$/.test(path)) {
    return send(readFileSync(root + path), path.endsWith('.css') ? 'text/css' : 'text/javascript');
  }
  if (path === api) {
    const statuses = new URL(req.url, 'http://127.0.0.1').searchParams.get('status')?.split(',');
    if (failDoneAfterClose && writes.length && statuses?.includes('closed')) {
      doneFailures++;
      res.statusCode = 503;
      return send(JSON.stringify({ success: false, message: 'Synthetic Done query failed' }));
    }
    const pending = cards.filter((item) => item.input.status === 'open');
    return data({
      inputs: cards.filter((item) => statuses?.includes(item.input.status)),
      pending_count: pending.length, next_cursor: null,
    });
  }
  if (card && path === `${api}/${id}`) return data(card);
  if (card && suffix === 'versions' && version === '1' && content === 'content' && path === `${api}/${id}/versions/1/content`) {
    res.setHeader('Content-Security-Policy', csp);
    return send('<!doctype html><title>Synthetic Input</title><p>Queue fixture content.</p>', 'text/html; charset=utf-8');
  }
  return deny();
}

export async function verifyQueue({ page, origin, until }) {
  const title = () => page.evaluate(`document.querySelector('[role="dialog"] h2')?.textContent.trim()`);
  const names = async () => {
    const { nodes } = await page.send('Accessibility.getFullAXTree');
    return nodes.filter((node) => !node.ignored && node.role?.value === 'button')
      .map((node) => node.name?.value ?? '');
  };
  let shortPhone;
  const activate = async (name, pointer = false, scope = '[role="dialog"]') => {
    await page.send('Page.bringToFront');
    const point = await page.evaluate(`(() => {
      const button = [...document.querySelectorAll(${JSON.stringify(scope + ' button')})].find(b =>
        (b.getAttribute('aria-label') ?? b.textContent.trim()) === ${JSON.stringify(name)} && b.checkVisibility());
      if (!button || button.disabled) return null;
      if (!${pointer}) {
        button.focus();
        return document.activeElement === button;
      }
      button.scrollIntoView({block: 'nearest'});
      const rect = button.getBoundingClientRect();
      const x = rect.left + rect.width / 2;
      const y = rect.top + rect.height / 2;
      const hit = document.elementFromPoint(x, y);
      if (${!!shortPhone}) {
        const label = (element) => {
          const control = element?.closest?.('button, h2');
          return {tag: element?.tagName, name: (control?.getAttribute('aria-label') ??
            control?.textContent)?.trim().slice(0, 64)};
        };
        const events = [];
        window.queuePointerTrace = {x, y, hit: label(hit), events};
        for (const type of ['pointerdown', 'pointerup', 'pointercancel', 'click', 'focusin', 'hashchange']) {
          window.addEventListener(type, event => {
            if (events.length === 12) events.shift();
            events.push({type, trusted: event.isTrusted, target: label(event.target),
              x: event.clientX, y: event.clientY, hash: location.hash.slice(0, 64)});
          }, true);
        }
      }
      return button.contains(hit) ? { x, y } : null;
    })()`);
    if (shortPhone) shortPhone.dispatchPoint = point;
    assert.ok(point, `Reach the ${name} button`);
    if (pointer) {
      const mouse = { ...point, button: 'left', clickCount: 1 };
      await page.send('Input.dispatchMouseEvent', { type: 'mousePressed', ...mouse });
      await page.send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...mouse });
    } else {
      const before = await page.evaluate(`(() => {
        if (!window.queueKeyEvents) {
          window.queueKeyEvents = [];
          for (const type of ['focusin', 'focusout', 'keydown', 'keypress', 'keyup', 'click']) {
            document.addEventListener(type, event => {
              const name = event.target.getAttribute?.('aria-label') ?? event.target.textContent?.trim();
              window.queueKeyEvents.push({type, key: event.key, trusted: event.isTrusted,
                target: event.target.tagName, name: name?.slice(0, 64)});
              if (window.queueKeyEvents.length > 12) window.queueKeyEvents.shift();
            }, true);
          }
        }
        window.queueKeyEvents.length = 0;
        return {focused: document.hasFocus(), visibility: document.visibilityState,
          active: document.activeElement?.tagName};
      })()`);
      const key = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 };
      await page.send('Input.dispatchKeyEvent', { type: 'keyDown', ...key, text: '\r' });
      await page.send('Input.dispatchKeyEvent', { type: 'keyUp', ...key });
      keyboard.push({ name, before, after: await page.evaluate(`({focused: document.hasFocus(),
        active: document.activeElement?.tagName, events: window.queueKeyEvents})`) });
    }
  };
  let visit = 0;
  const open = async (i, width, height, only = false, startFromList = false) => {
    reset(only ? i : undefined);
    await page.send('Emulation.setDeviceMetricsOverride', {
      width, height, deviceScaleFactor: 1, mobile: width < 768,
    });
    const search = `?visit=${++visit}`;
    const hash = `#input=${ids[i]}`;
    const navigation = await page.send('Page.navigate', { url: `${origin}/queue${search}${startFromList ? '' : hash}` });
    assert.equal(navigation.errorText, undefined, 'Navigate to the synthetic queue');
    await until(async () => {
      const { frameTree } = await page.send('Page.getFrameTree');
      return frameTree.frame.loaderId === navigation.loaderId && frameTree.frame.url.includes(search);
    }, 'commit the fresh fixture');
    if (startFromList) {
      await until(() => page.evaluate(`!!document.querySelector('section[aria-label="Inputs"] button')`), 'render the Inputs list');
      await activate('Done', true, 'section[aria-label="Inputs"]');
      await until(() => page.evaluate(`(() => {
        const list = document.querySelector('section[aria-label="Inputs"]');
        const done = [...list.querySelectorAll('button')].find(button => button.textContent.trim() === 'Done');
        return done?.getAttribute('aria-pressed') === 'true' && list.innerText.includes('Your decision pages will appear here.');
      })()`), 'load the active Done query');
      await page.evaluate(`void (location.hash = ${JSON.stringify(hash)})`);
    }
    await until(async () => (await title()) === titles[i], `render ${titles[i]}`);
    await until(() => page.evaluate(`!!document.querySelector('[role="dialog"] aside')`), 'render Input actions');
    const position = only ? '1 of 1' : `${i + 1} of 3`;
    await until(() => page.evaluate(`document.querySelector('[role="dialog"]').innerText.includes(${JSON.stringify(position)})`), 'load the pending position');
  };
  const headingFocused = (expected) => until(() => page.evaluate(`
    document.activeElement === document.querySelector('[role="dialog"] h2') &&
    document.activeElement.textContent.trim() === ${JSON.stringify(expected)}
  `), `focus the ${expected} heading`);
  const results = [];
  const keyboard = [];
  async function test(label, run) {
    keyboard.length = 0;
    try {
      await run();
      assert.equal(unexpectedWrite, false, 'The fixture allows only the owner close route');
      results.push({ label, pass: true });
    } catch (error) {
      let focus;
      try {
        focus = await page.evaluate(`({focused: document.hasFocus(), visibility: document.visibilityState,
          active: document.activeElement?.tagName, events: window.queueKeyEvents?.slice(-12)})`);
      } catch {
        // Keep the original failure even after the page detaches.
      }
      if (shortPhone) {
        shortPhone.closeRequests = closeRequests;
        shortPhone.writes = writes.slice(0, 4);
        try {
          shortPhone.browser = await page.evaluate(`(() => {
            const dialog = document.querySelector('[role="dialog"]');
            const active = document.activeElement;
            return {hash: location.hash.slice(0, 64),
              heading: dialog?.querySelector('h2')?.textContent.trim().slice(0, 64),
              activeName: (active?.getAttribute('aria-label') ??
                active?.closest('button, h2')?.textContent)?.trim().slice(0, 64),
              alert: dialog?.querySelector('aside [role="alert"]')?.textContent.trim().slice(0, 160),
              pointer: window.queuePointerTrace};
          })()`);
        } catch {
          // Keep the server evidence if the page detaches.
        }
      }
      results.push({ label, pass: false, message: error.message, focus, keyboard: keyboard.slice(-4), shortPhone });
    } finally {
      shortPhone = undefined;
    }
  }

  await test('closing the last desktop card selects the nearest earlier pending card', async () => {
    await open(2, 1280, 900);
    assert.ok((await names()).includes('Close Input'), 'Close Input has an accessible name');
    await activate('Close Input', true);
    await until(() => writes.length === 1, 'persist the close');
    assert.deepEqual(writes, [{ id: ids[2], reason: null }]);
    const selected = await until(() => page.evaluate(`location.hash !== '#input=${ids[2]}' && location.hash`), 'advance after Close');
    assert.equal(selected, `#input=${ids[1]}`, 'Close must select the nearest earlier pending card');
    await headingFocused(titles[1]);
  });
  await test('a failing active Done query cannot block advance after Close', async () => {
    await open(2, 1280, 900, false, true);
    failDoneAfterClose = true;
    await activate('Close Input', true);
    await until(() => doneFailures > 0, 'refetch and fail the unrelated Done query');
    assert.deepEqual(writes, [{ id: ids[2], reason: null }]);
    await headingFocused(titles[1]);
  });
  await test('an informational card offers a separate Mark as seen action', async () => {
    await open(1, 1280, 900);
    assert.ok((await names()).includes('Mark as seen'), 'Informational Input must offer an accessible Mark as seen button');
    await activate('Mark as seen');
    await until(() => writes.length === 1, 'persist the acknowledgment');
    assert.deepEqual(writes, [{ id: ids[1], reason: 'marked_as_seen' }]);
    await headingFocused(titles[2]);
  });
  await test('closing the only pending card focuses completion', async () => {
    await open(0, 1280, 900, true);
    await activate('Close Input');
    await headingFocused('All caught up');
    assert.deepEqual(writes, [{ id: ids[0], reason: null }]);
    assert.equal(await page.evaluate('location.hash'), '');
  });
  await test('a failed close leaves the card selected', async () => {
    await open(0, 1280, 900);
    rejectClose = true;
    await activate('Close Input');
    await until(() => page.evaluate(`document.querySelector('[role="dialog"]').innerText.includes('Synthetic close failed')`), 'display the close failure');
    assert.equal(await title(), titles[0]);
    assert.equal(await page.evaluate('location.hash'), `#input=${ids[0]}`);
    assert.deepEqual(writes, []);
  });
  await test('phone queue controls and counts are visible and accessible', async () => {
    await open(1, 390, 844);
    const buttons = await names();
    const { width, text } = await page.evaluate(`({width: innerWidth, text: document.querySelector('[role="dialog"]').innerText.replace(/\\s+/g, ' ')})`);
    assert.equal(width, 390, 'Phone uses the narrow layout');
    const missing = [
      !buttons.some((name) => /^Previous\b/.test(name)) && 'Previous',
      !buttons.some((name) => /^Next\b/.test(name)) && 'Next',
      !buttons.includes('View list') && 'View list',
      !/\b3 pending\b/i.test(text) && 'pending count',
      !/\b2 of 3\b/i.test(text) && 'position',
    ].filter(Boolean);
    assert.deepEqual(missing, [], 'Phone queue controls must be visible and accessible');
    const next = buttons.find((name) => /^Next\b/.test(name));
    await activate(next);
    await until(async () => (await title()) === titles[2], 'open the next phone card');
    const previous = (await names()).find((name) => /^Previous\b/.test(name));
    await activate(previous);
    await until(async () => (await title()) === titles[1], 'open the previous phone card');
    await activate('View list');
    await until(() => page.evaluate(`(() => {
      const list = document.querySelector('section[aria-label="Inputs"]');
      return list?.checkVisibility() && document.activeElement === list && !document.querySelector('[role="dialog"]');
    })()`), 'focus the full phone list');
  });
  await test('queue controls and card actions remain reachable on a short phone', async () => {
    await open(1, 390, 250);
    assert.equal(await page.evaluate('innerHeight'), 250);
    const queueControlsReachable = () => page.evaluate(`(() => {
      const controls = [...document.querySelectorAll('nav[aria-label="Open Inputs"] button')];
      return controls.length === 3 && controls.every(button => {
        const {x, y, width, height} = button.getBoundingClientRect();
        return button.contains(document.elementFromPoint(x + width / 2, y + height / 2));
      });
    })()`);
    assert.ok(await queueControlsReachable(), 'Reach all queue controls before scrolling');
    await page.evaluate(`(() => {
      const action = [...document.querySelectorAll('aside button')].find(
        button => button.textContent.trim() === 'Mark as seen');
      action.scrollIntoView({block: 'nearest'});
    })()`);
    assert.ok(await queueControlsReachable(), 'Reach all queue controls with the action in view');
    shortPhone = {};
    await activate('Mark as seen', true);
    await headingFocused(titles[2]);
    assert.deepEqual(writes, [{ id: ids[1], reason: 'marked_as_seen' }]);
    assert.ok(await queueControlsReachable(), 'Reach all queue controls after scrolling');
  });
  console.log(JSON.stringify({ inputQueueBrowser: results }, null, 2));
  assert.ok(results.every((result) => result.pass), 'Input queue browser acceptance');
}
