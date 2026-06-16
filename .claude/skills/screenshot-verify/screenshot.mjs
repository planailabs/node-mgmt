#!/usr/bin/env node
// Self-contained headless-Chrome screenshotter for the plan.ai SPA preview.
//
// Launches its OWN fresh-profile Chrome (so a previous app's service worker —
// e.g. Open-WebUI's PWA on :8080 — can't hijack the page), drives it over the
// DevTools protocol with Node's global WebSocket (no npm deps), optionally runs
// a click expression, screenshots, then tears the browser down.
//
// Usage:
//   node screenshot.mjs --url http://127.0.0.1:8080 --out /tmp/shot.png \
//        [--click "() => document.querySelector('button')?.click()"] \
//        [--wait 2000] [--size 1100,820] [--ready "header.topbar"]
//
// --click  : a JS arrow-fn evaluated in the page AFTER it hydrates; use it to
//            open menus/dropdowns before the shot. Should return truthy.
// --ready  : CSS selector polled until present before screenshotting
//            (default: "body"). For this SPA use "header.topbar".
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const argv = process.argv.slice(2);
const opt = {};
for (let i = 0; i < argv.length; i++) {
  if (argv[i].startsWith('--')) { opt[argv[i].slice(2)] = (argv[i + 1] && !argv[i + 1].startsWith('--')) ? argv[++i] : true; }
}

const url = opt.url;
const out = opt.out;
if (!url || !out) { console.error('need --url and --out'); process.exit(2); }
const ready = opt.ready || 'body';
const waitMs = parseInt(opt.wait || '2000', 10);
const [w, h] = (opt.size || '1100,820').split(',').map(Number);
const click = opt.click;

const CHROME = process.env.CHROME ||
  ['/run/current-system/sw/bin/google-chrome', '/usr/bin/google-chrome',
   '/usr/bin/chromium', '/run/current-system/sw/bin/chromium'].find(Boolean);

const debugPort = 9000 + (process.pid % 800);
const profile = mkdtempSync(join(tmpdir(), 'cdp-prof-'));
const chrome = spawn(CHROME, [
  '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
  `--remote-debugging-port=${debugPort}`, `--user-data-dir=${profile}`,
  `--window-size=${w},${h}`, 'about:blank',
], { stdio: 'ignore' });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function cdpVersion() {
  for (let i = 0; i < 60; i++) {
    try { const r = await fetch(`http://127.0.0.1:${debugPort}/json/version`); if (r.ok) return; } catch {}
    await sleep(500);
  }
  throw new Error('chrome CDP did not come up');
}

async function main() {
  await cdpVersion();
  const t = await (await fetch(`http://127.0.0.1:${debugPort}/json/new?` + encodeURIComponent(url), { method: 'PUT' })).json();
  const ws = new WebSocket(t.webSocketDebuggerUrl);
  let id = 0; const pending = new Map();
  ws.addEventListener('message', (ev) => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { pending.get(m.id)(m.result); pending.delete(m.id); } });
  const send = (method, params = {}) => new Promise((res) => { const i = ++id; pending.set(i, res); ws.send(JSON.stringify({ id: i, method, params })); });
  await new Promise((res) => ws.addEventListener('open', res));

  await send('Page.enable'); await send('Runtime.enable');
  await send('Page.navigate', { url });
  // poll for hydration / the ready selector
  for (let i = 0; i < 60; i++) {
    const r = await send('Runtime.evaluate', { expression: `!!document.querySelector(${JSON.stringify(ready)})`, returnByValue: true });
    if (r.result && r.result.value) break;
    await sleep(500);
  }
  await sleep(waitMs);
  if (click) {
    await send('Runtime.evaluate', { expression: `(${click})()`, returnByValue: true });
    await sleep(800);
  }
  const { data } = await send('Page.captureScreenshot', { format: 'png' });
  writeFileSync(out, Buffer.from(data, 'base64'));
  ws.close();
  console.log('wrote', out);
}

main()
  .catch((e) => { console.error(e.message || e); process.exitCode = 1; })
  .finally(() => { try { chrome.kill('SIGKILL'); } catch {} try { rmSync(profile, { recursive: true, force: true }); } catch {} });
