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
let unexpectedWrite;

function reset() {
  writes = [];
  unexpectedWrite = false;
  cards = ids.map((id, i) => ({
    input: {
      id, workspace_id: workspace, external_key: `fixture-${i}`, creator: 'fixture',
      show_at: null, expires_at: null, priority: 'P2', current_version: 1,
      status: 'open', sequence: 1, created_at: timestamp, updated_at: timestamp,
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
  const activate = async (name, pointer = false) => {
    const point = await page.evaluate(`(() => {
      const button = [...document.querySelectorAll('[role="dialog"] button')].find(b =>
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
      return button.contains(document.elementFromPoint(x, y)) ? { x, y } : null;
    })()`);
    assert.ok(point, `Reach the ${name} button`);
    if (pointer) {
      const mouse = { ...point, button: 'left', clickCount: 1 };
      await page.send('Input.dispatchMouseEvent', { type: 'mousePressed', ...mouse });
      await page.send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...mouse });
    } else {
      const key = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 };
      await page.send('Input.dispatchKeyEvent', { type: 'keyDown', ...key });
      await page.send('Input.dispatchKeyEvent', { type: 'keyUp', ...key });
    }
  };
  let visit = 0;
  const open = async (i, width, height) => {
    reset();
    await page.send('Emulation.setDeviceMetricsOverride', {
      width, height, deviceScaleFactor: 1, mobile: width < 768,
    });
    const search = `?visit=${++visit}`;
    const navigation = await page.send('Page.navigate', { url: `${origin}/queue${search}#input=${ids[i]}` });
    assert.equal(navigation.errorText, undefined, 'Navigate to the synthetic queue');
    await until(async () => {
      const { frameTree } = await page.send('Page.getFrameTree');
      return frameTree.frame.loaderId === navigation.loaderId && frameTree.frame.url.includes(search);
    }, 'commit the fresh fixture');
    await until(async () => (await title()) === titles[i], `render ${titles[i]}`);
    await until(() => page.evaluate(`!!document.querySelector('[role="dialog"] aside')`), 'render Input actions');
    await until(() => page.evaluate(`document.querySelector('[role="dialog"]').innerText.includes(' of 3')`), 'load the pending position');
  };
  const results = [];
  async function test(label, run) {
    try {
      await run();
      assert.equal(unexpectedWrite, false, 'The fixture allows only the owner close route');
      results.push({ label, pass: true });
    } catch (error) {
      results.push({ label, pass: false, message: error.message });
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
  });
  await test('an informational card offers a separate Mark as seen action', async () => {
    await open(1, 1280, 900);
    assert.ok((await names()).includes('Mark as seen'), 'Informational Input must offer an accessible Mark as seen button');
    await activate('Mark as seen');
    await until(() => writes.length === 1, 'persist the acknowledgment');
    assert.deepEqual(writes, [{ id: ids[1], reason: 'marked_as_seen' }]);
    await until(async () => (await title()) === titles[2], 'advance after Mark as seen');
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
    await until(() => page.evaluate(`document.querySelector('section[aria-label="Inputs"]')?.checkVisibility() && !document.querySelector('[role="dialog"]')`), 'open the full phone list');
  });
  console.log(JSON.stringify({ inputQueueBrowser: results }, null, 2));
  assert.ok(results.every((result) => result.pass), 'Input queue browser acceptance');
}
