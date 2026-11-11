#!/usr/bin/env node
// CI-only: the workflow supplies the OS sandbox and aggregate resource limits.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeSync } from 'node:fs';
import { createServer } from 'node:http';
import { stripTypeScriptTypes } from 'node:module';
import { spawn } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';

const stage = (name) => writeSync(2, `[input-browser] ${name}\n`);
stage('node-entry');
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
stage('source-read');
const bridge = stripTypeScriptTypes(
  sources['bridge.ts'].replace(/^import .*;\r?\n/gm, ''),
);
stage('typescript-stripped');
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
    this.sessions = new Set();
    this.attachmentError = null;
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
      const sessionId = message.sessionId;
      if (message.method === 'Target.attachedToTarget') {
        const attached = message.params.sessionId;
        this.sessions.add(attached);
        if (this.sessions.size > 16) this.attachmentError = new Error('CDP session limit');
        else this.enableFrames(attached).catch((error) => { this.attachmentError = error; });
      }
      if (message.method === 'Target.detachedFromTarget') {
        this.sessions.delete(message.params.sessionId);
        for (const [key, context] of this.contexts) {
          if (context.sessionId === message.params.sessionId) this.contexts.delete(key);
        }
      }
      if (message.method === 'Runtime.executionContextCreated') {
        const context = { ...message.params.context, sessionId };
        this.contexts.set(`${sessionId ?? ''}:${context.id}`, context);
      }
      if (message.method === 'Runtime.executionContextDestroyed') {
        this.contexts.delete(`${sessionId ?? ''}:${message.params.executionContextId}`);
      }
      if (message.method === 'Runtime.executionContextsCleared') {
        for (const [key, context] of this.contexts) {
          if (context.sessionId === sessionId) this.contexts.delete(key);
        }
      }
    });
  }
  async enableFrames(sessionId) {
    await this.send('Page.enable', {}, sessionId);
    await this.send('Runtime.enable', {}, sessionId);
    await this.send('Target.setAutoAttach', {
      autoAttach: true, waitForDebuggerOnStart: false, flatten: true,
      filter: [{ type: 'iframe', exclude: false }, { exclude: true }],
    }, sessionId);
  }
  async send(method, params = {}, sessionId) {
    await this.ready;
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP timeout: ${method}`));
      }, 5000);
      this.pending.set(id, { resolve, reject, timer });
      this.socket.send(JSON.stringify({ id, method, params, sessionId }));
    });
  }
  async evaluate(expression, context) {
    const result = await this.send('Runtime.evaluate', {
      expression, contextId: context?.id, returnByValue: true,
    }, context?.sessionId);
    assert.ok(!result.exceptionDetails, JSON.stringify(result.exceptionDetails));
    return result.result.value;
  }
  close() { this.socket.close(); }
}

async function until(check, label, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await check();
    if (value) return value;
    await delay(50);
  }
  throw new Error(`Timeout: ${label}`);
}

let chrome;
let chromeStderr = '';
let page;
const clients = [];
try {
  stage('fixture-listen-start');
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  origin = `http://127.0.0.1:${server.address().port}`;
  stage(`fixture-listening ${origin}`);
  const flags = [
    '--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
    '--disable-component-update', '--disable-sync', '--metrics-recording-only',
    '--disable-breakpad', '--disable-crash-reporter', '--disable-extensions',
    '--user-data-dir=/scratch/profile', '--disk-cache-dir=/scratch/cache',
    '--disk-cache-size=1048576', '--remote-debugging-address=127.0.0.1',
    '--remote-debugging-port=0', '--window-size=420,800', 'about:blank',
  ];
  stage('chrome-spawn-start');
  chrome = spawn('/opt/google/chrome/chrome', flags, { stdio: ['ignore', 'ignore', 'pipe'] });
  stage(`chrome-spawned pid=${chrome.pid}`);
  let spawnError;
  chrome.once('error', (error) => { spawnError = error; stage(`chrome-spawn-error ${error.message}`); });
  chrome.once('exit', (code, signal) => stage(`chrome-exit code=${code} signal=${signal}`));
  chrome.stderr.on('data', (data) => {
    const text = data.toString();
    const remaining = Math.max(0, 16000 - chromeStderr.length);
    if (remaining) writeSync(2, text.slice(0, remaining));
    chromeStderr = (chromeStderr + text).slice(-16000);
  });
  // Cold Chrome startup receives a separate bound inside the 180-second OS timeout.
  const chromeStartupTimeoutMs = 30000;
  stage(`chrome-cdp-wait timeoutMs=${chromeStartupTimeoutMs}`);
  const endpoint = await until(() => {
    if (spawnError) throw spawnError;
    assert.equal(chrome.exitCode, null, chromeStderr);
    assert.equal(chrome.signalCode, null, chromeStderr);
    return chromeStderr.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1];
  }, 'Chrome CDP endpoint', chromeStartupTimeoutMs);
  stage('chrome-cdp-ready');
  const browser = new CDP(endpoint);
  clients.push(browser);
  const version = await browser.send('Browser.getVersion');
  stage(`browser-version ${version.product}`);
  const port = new URL(endpoint).port;
  const list = () => fetch(`http://127.0.0.1:${port}/json/list`).then((r) => r.json());
  const initial = (await list()).find((target) => target.type === 'page');
  assert.ok(initial);
  page = new CDP(initial.webSocketDebuggerUrl);
  clients.push(page);
  await page.enableFrames();
  stage('page-sessions-ready');
  await page.send('Emulation.setDeviceMetricsOverride', { width: 420, height: 800, deviceScaleFactor: 1, mobile: false });
  await page.send('Page.navigate', { url: origin });
  await until(async () => (await page.evaluate('window.state?.loaded')) === 1, 'Input mount');
  stage('input-mounted');
  async function inputFrame() {
    if (page.attachmentError) throw page.attachmentError;
    for (const sessionId of [undefined, ...page.sessions]) {
      const { frameTree } = await page.send('Page.getFrameTree', {}, sessionId);
      const frames = [frameTree];
      while (frames.length) {
        const entry = frames.pop();
        if (entry.frame.url.split('#')[0] === origin + contentPath) return entry.frame;
        frames.push(...(entry.childFrames ?? []));
      }
    }
  }
  const inner = await until(inputFrame, 'Input frame in the actual embedding');
  const context = await until(() => {
    if (page.attachmentError) throw page.attachmentError;
    return [...page.contexts.values()].find((c) => c.auxData?.frameId === inner.id && c.auxData.isDefault);
  }, 'Input execution context');
  const evaluate = (expression) => page.evaluate(expression, context);
  const mountId = await evaluate('window.__mountId');
  assert.ok(mountId);
  async function mounted() {
    assert.deepEqual(await page.evaluate('state'), { outerLoads: 1, loaded: 1, navigated: 0, unloads: 0 });
    assert.equal(await evaluate('window.__mountId'), mountId);
    assert.equal(await evaluate('window.__unsafeExecuted === true'), false);
    const current = await inputFrame();
    assert.equal(current?.id, inner.id);
    assert.equal(current?.loaderId, inner.loaderId, 'The Input has one document load');
    assert.equal(requests.filter((url) => url === contentPath).length, 1);
  }
  async function click(id, button = 'left') {
    const point = await evaluate(`(()=>{const a=document.getElementById(${JSON.stringify(id)});
      a.scrollIntoView({block:'center'});const r=(a.tagName==='AREA'?document.querySelector('img[usemap]'):a).getBoundingClientRect();
      return {x:r.x+r.width/2,y:r.y+r.height/2};})()`);
    assert.ok(point.x > 0 && point.y > 0 && point.x < 420 && point.y < 800);
    assert.deepEqual(await page.evaluate(`(()=>{const outer=document.querySelector('iframe');
      const inner=outer.contentDocument.querySelector('iframe');
      return [outer,inner].map(f=>{const r=f.getBoundingClientRect();return [r.x,r.y];});})()`),
      [[0, 0], [0, 0]], 'Frame coordinates match the top-level viewport');
    await page.send('Input.dispatchMouseEvent', { type: 'mousePressed', ...point, button, clickCount: 1 });
    await page.send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...point, button, clickCount: 1 });
  }
  const results = [];
  async function popup(id, activate) {
    stage(`acceptance ${id}`);
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
} catch (error) {
  stage(`failure ${error.stack ?? error}`);
  stage(`chrome-state exit=${chrome?.exitCode} signal=${chrome?.signalCode}`);
  if (chromeStderr) writeSync(2, `[input-browser] Chrome stderr tail\n${chromeStderr}\n`);
  throw error;
} finally {
  stage('cleanup-start');
  for (const client of clients) client.close();
  chrome?.kill('SIGTERM');
  server.closeAllConnections();
  server.close();
}
