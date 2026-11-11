#!/usr/bin/env node
// CI-only: the workflow supplies the OS sandbox and aggregate resource limits.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { stripTypeScriptTypes } from 'node:module';
import { spawn } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';

assert.equal(process.env.INPUT_BROWSER_SANDBOX, '1', 'Use the isolated CI job');
assert.equal(process.cwd(), '/scratch');
const sources = Object.fromEntries(
  ['runtime.js', 'bridge.ts', 'inputs.rs'].map((name) => [
    name,
    readFileSync(`/fixture/${name}`, 'utf8'),
  ]),
);
const csp = sources['inputs.rs'].match(/pub const INPUT_CONTENT_CSP: &str = "([^"]+)";/)?.[1];
assert.ok(csp, 'Read the exact backend CSP');
const bridge = stripTypeScriptTypes(
  sources['bridge.ts'].replace(/^import .*;\r?\n/gm, ''),
);
const contentPath = '/api/workspaces/synthetic/inputs/links/versions/1/content';
const requests = [];
let origin;
const server = createServer((req, res) => {
  requests.push(req.url);
  res.setHeader('Cache-Control', 'no-store');
  res.setHeader('Content-Type', 'text/html; charset=utf-8');
  if (req.url === '/bridge.js') {
    res.setHeader('Content-Type', 'text/javascript');
    return res.end(`const mermaidSource = '';\n${bridge}`);
  }
  if (req.url === '/') {
    return res.end(`<!doctype html><style>html,body{margin:0}iframe{border:0;width:100vw;height:100vh}</style>
<script>window.state={outerLoads:0,loaded:0,navigated:0,unloads:0};
addEventListener('message',e=>{if(e.data.type==='bc.input.loaded')state.loaded++;
if(e.data.type==='bc.input.navigated')state.navigated++;
if(e.data.type==='fixture.unload')state.unloads++;});</script>
<script type="module">import {createInputBridge,inputEmbeddingDocument} from '/bridge.js';
const frame=document.createElement('iframe');frame.title='Synthetic Input';
frame.onload=()=>state.outerLoads++;
frame.srcdoc=inputEmbeddingDocument(${JSON.stringify(origin + contentPath)},createInputBridge('links',1),'synthetic');
document.body.append(frame);</script>`);
  }
  if (req.url === contentPath) {
    res.setHeader('Content-Security-Policy', csp);
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader('Referrer-Policy', 'no-referrer');
    return res.end(`<!doctype html><html><head><style>
body{margin:8px}a{display:block;padding:5px}svg a{display:inline}svg{display:block}
</style></head><body><h1>Synthetic Input links</h1>
<a id="primary" href="${origin}/target?primary"><span>Primary click</span></a>
<a id="keyboard" href="${origin}/target?keyboard">Keyboard Enter</a>
<a id="middle" href="${origin}/target?middle">Middle click</a>
<a id="javascript" href="javascript:window.__unsafeExecuted=true">Blocked JavaScript</a>
<a id="data" href="data:text/html,unsafe">Blocked data</a>
<a id="relative" href="/target?relative">Blocked relative</a>
<a id="fragment" href="#section" target="_blank">Local fragment</a>
<svg width="250" height="45" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
<a id="svg" xlink:href="${origin}/target?svg"><rect width="250" height="40" fill="#eee"/><text x="10" y="25">SVG link</text></a></svg>
<img usemap="#links" alt="Synthetic image map" width="250" height="40" src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==">
<map name="links"><area id="area" shape="rect" coords="0,0,250,40" href="${origin}/target?area" alt="Image map link"></map>
<h2 id="section">Local section</h2>
<script>window.__mountId=crypto.randomUUID();window.__events=[];
addEventListener('pagehide',()=>top.postMessage({type:'fixture.unload'},'*'));</script>
<script>${sources['runtime.js']}</script>
<script>for(const type of ['click','auxclick'])document.addEventListener(type,e=>{
const a=e.target.closest?.('a,area');if(a)__events.push({id:a.id,type:e.type,trusted:e.isTrusted,prevented:e.defaultPrevented});});</script>
</body></html>`);
  }
  if (req.url?.startsWith('/target?')) return res.end('<!doctype html><title>Synthetic target</title>Target');
  res.statusCode = 404;
  res.end('Fixture route unavailable');
});

class CDP {
  constructor(url) {
    this.pending = new Map();
    this.contexts = new Map();
    this.id = 0;
    this.socket = new WebSocket(url);
    this.ready = new Promise((resolve, reject) => {
      this.socket.addEventListener('open', resolve, { once: true });
      this.socket.addEventListener('error', reject, { once: true });
    });
    this.socket.addEventListener('message', ({ data }) => {
      const message = JSON.parse(data);
      if (message.id) {
        const pending = this.pending.get(message.id);
        if (!pending) return;
        this.pending.delete(message.id);
        clearTimeout(pending.timer);
        if (message.error) pending.reject(new Error(JSON.stringify(message.error)));
        else pending.resolve(message.result);
      }
      if (message.method === 'Runtime.executionContextCreated') {
        this.contexts.set(message.params.context.id, message.params.context);
      }
      if (message.method === 'Runtime.executionContextDestroyed') {
        this.contexts.delete(message.params.executionContextId);
      }
      if (message.method === 'Runtime.executionContextsCleared') this.contexts.clear();
    });
  }
  async send(method, params = {}) {
    await this.ready;
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP timeout: ${method}`));
      }, 5000);
      this.pending.set(id, { resolve, reject, timer });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }
  async evaluate(expression, contextId) {
    const result = await this.send('Runtime.evaluate', { expression, contextId, returnByValue: true });
    assert.ok(!result.exceptionDetails, JSON.stringify(result.exceptionDetails));
    return result.result.value;
  }
  close() { this.socket.close(); }
}

async function until(check, label) {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    const value = await check();
    if (value) return value;
    await delay(50);
  }
  throw new Error(`Timeout: ${label}`);
}

let chrome;
let page;
const clients = [];
try {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  origin = `http://127.0.0.1:${server.address().port}`;
  const flags = [
    '--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
    '--disable-component-update', '--disable-sync', '--metrics-recording-only',
    '--disable-breakpad', '--disable-crash-reporter', '--disable-extensions',
    '--user-data-dir=/scratch/profile', '--disk-cache-dir=/scratch/cache',
    '--disk-cache-size=1048576', '--remote-debugging-address=127.0.0.1',
    '--remote-debugging-port=0', '--window-size=420,800', 'about:blank',
  ];
  chrome = spawn('/opt/google/chrome/chrome', flags, { stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  chrome.stderr.on('data', (data) => { stderr = (stderr + data).slice(-16000); });
  const endpoint = await until(() => {
    assert.equal(chrome.exitCode, null, stderr);
    return stderr.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1];
  }, 'Chrome CDP endpoint');
  const browser = new CDP(endpoint);
  clients.push(browser);
  const version = await browser.send('Browser.getVersion');
  const port = new URL(endpoint).port;
  const list = () => fetch(`http://127.0.0.1:${port}/json/list`).then((r) => r.json());
  const initial = (await list()).find((target) => target.type === 'page');
  assert.ok(initial);
  page = new CDP(initial.webSocketDebuggerUrl);
  clients.push(page);
  await page.send('Page.enable');
  await page.send('Runtime.enable');
  await page.send('Emulation.setDeviceMetricsOverride', { width: 420, height: 800, deviceScaleFactor: 1, mobile: false });
  await page.send('Page.navigate', { url: origin });
  await until(async () => (await page.evaluate('window.state?.loaded')) === 1, 'Input mount');
  const tree = await page.send('Page.getFrameTree');
  const inner = tree.frameTree.childFrames?.[0]?.childFrames?.[0]?.frame;
  assert.ok(inner, 'The actual embedding contains the Input frame');
  const context = await until(() => [...page.contexts.values()].find((c) => c.auxData?.frameId === inner.id && c.auxData.isDefault), 'Input execution context');
  const evaluate = (expression) => page.evaluate(expression, context.id);
  const mountId = await evaluate('window.__mountId');
  assert.ok(mountId);
  async function mounted() {
    assert.deepEqual(await page.evaluate('state'), { outerLoads: 1, loaded: 1, navigated: 0, unloads: 0 });
    assert.equal(await evaluate('window.__mountId'), mountId);
    assert.equal(await evaluate('window.__unsafeExecuted === true'), false);
    const current = (await page.send('Page.getFrameTree')).frameTree.childFrames?.[0]?.childFrames?.[0]?.frame;
    assert.equal(current?.id, inner.id);
    assert.equal(current?.loaderId, inner.loaderId, 'The Input has one document load');
    assert.equal(requests.filter((url) => url === contentPath).length, 1);
  }
  async function click(id, button = 'left') {
    const point = await evaluate(`(()=>{const a=document.getElementById(${JSON.stringify(id)});
      a.scrollIntoView({block:'center'});const r=(a.tagName==='AREA'?document.querySelector('img[usemap]'):a).getBoundingClientRect();
      return {x:r.x+r.width/2,y:r.y+r.height/2};})()`);
    assert.ok(point.x > 0 && point.y > 0);
    await page.send('Input.dispatchMouseEvent', { type: 'mousePressed', ...point, button, clickCount: 1 });
    await page.send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...point, button, clickCount: 1 });
  }
  const results = [];
  async function popup(id, activate) {
    const before = (await list()).filter((t) => t.type === 'page');
    assert.equal(before.length, 1);
    await page.send('Page.bringToFront');
    await activate();
    const target = await until(async () => (await list()).find((t) => t.type === 'page' && t.id !== initial.id && t.url === `${origin}/target?${id}`), `${id} popup`);
    await delay(200);
    const after = (await list()).filter((t) => t.type === 'page');
    assert.equal(after.length, before.length + 1, 'Exactly one new browsing context');
    const popupPage = new CDP(target.webSocketDebuggerUrl);
    clients.push(popupPage);
    assert.equal(await popupPage.evaluate('window.opener === null'), true);
    const events = await evaluate(`__events.filter(e=>e.id===${JSON.stringify(id)})`);
    assert.equal(events.length, 1);
    assert.equal(events[0].trusted, true, 'Chrome dispatches a trusted native activation');
    assert.equal(events[0].prevented, false);
    await mounted();
    await browser.send('Target.closeTarget', { targetId: target.id });
    await until(async () => (await list()).filter((t) => t.type === 'page').length === 1, 'Popup close');
    results.push({ case: id, contextsOpened: 1, openerNull: true, inputMounted: true, events });
  }
  await popup('primary', () => click('primary'));
  await popup('keyboard', async () => {
    await evaluate("document.getElementById('keyboard').focus()");
    await page.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
    await page.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
  });
  await popup('middle', () => click('middle', 'middle'));
  await popup('svg', () => click('svg'));
  await popup('area', () => click('area'));
  for (const id of ['javascript', 'data', 'relative']) {
    await page.send('Page.bringToFront');
    await click(id);
    await delay(200);
    assert.equal((await list()).filter((t) => t.type === 'page').length, 1);
    assert.deepEqual(await evaluate(`__events.filter(e=>e.id==='${id}').map(e=>[e.trusted,e.prevented])`), [[true, true]]);
    await mounted();
    results.push({ case: id, contextsOpened: 0, blocked: true, inputMounted: true });
  }
  await click('fragment');
  await until(async () => (await evaluate('location.hash')) === '#section', 'Local fragment');
  assert.equal((await list()).filter((t) => t.type === 'page').length, 1);
  await mounted();
  assert.equal(requests.some((url) => url.includes('relative')), false);
  results.push({ case: 'fragment', contextsOpened: 0, localHash: '#section', inputMounted: true });
  console.log(JSON.stringify({
    browser: version,
    sourceSha256: Object.fromEntries(Object.entries(sources).map(([name, source]) => [name, createHash('sha256').update(source).digest('hex')])),
    csp, viewport: { width: 420, height: 800 }, flags,
    environment: process.env, requests, results,
  }, null, 2));
} finally {
  for (const client of clients) client.close();
  chrome?.kill('SIGTERM');
  server.closeAllConnections();
  server.close();
}
