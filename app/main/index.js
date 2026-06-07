'use strict';
const { app, BrowserWindow, ipcMain } = require('electron');
const paths = require('./paths');
const cfg = require('./config');
const loader = require('./loader');
const { Supervisor } = require('./supervisor');

let win = null;
let supervisor = null;

function createWindow() {
  win = new BrowserWindow({
    width: 1200,
    height: 820,
    minWidth: 900,
    minHeight: 600,
    backgroundColor: '#0f0e0c',
    title: 'plan.ai',
    show: false,
    webPreferences: {
      preload: paths.preload(),
      contextIsolation: true,
      nodeIntegration: false,
      webviewTag: true, // the dashboard embeds Open-WebUI via <webview>
    },
  });
  win.removeMenu();
  win.loadFile(paths.rendererIndex());
  win.once('ready-to-show', () => win.show());
  win.on('closed', () => { win = null; });

  // CI/dev smoke: capture the dashboard (and optionally the embedded WebUI)
  // then exit. PLANAI_CAPTURE=/dash.png [PLANAI_CAPTURE_WEBUI=/webui.png].
  if (process.env.PLANAI_CAPTURE) {
    const fs = require('fs');
    const snap = async (file) => {
      try { fs.writeFileSync(file, (await win.webContents.capturePage()).toPNG()); }
      catch (e) { console.error('capture failed', e); }
    };
    win.webContents.once('did-finish-load', () => {
      setTimeout(async () => {
        await snap(process.env.PLANAI_CAPTURE);
        if (process.env.PLANAI_CAPTURE_WEBUI) {
          const tab = process.env.PLANAI_CAPTURE_TAB || 'tab-webui';
          try { await win.webContents.executeJavaScript(`document.getElementById('${tab}')?.click()`); }
          catch {}
          await new Promise((r) => setTimeout(r, Number(process.env.PLANAI_CAPTURE_WEBUI_DELAY || 7000)));
          await snap(process.env.PLANAI_CAPTURE_WEBUI);
        }
        app.quit();
      }, Number(process.env.PLANAI_CAPTURE_DELAY || 2500));
    });
  }
}

function wireIpc() {
  const send = (ch, payload) => { if (win && !win.isDestroyed()) win.webContents.send(ch, payload); };
  supervisor.on('status', (e) => send('svc:status', e));
  supervisor.on('log', (e) => send('svc:log', e));

  ipcMain.handle('app:info', () => ({
    webuiUrl: cfg.webuiUrl,
    ollamaPort: cfg.OLLAMA_PORT,
    webuiPort: cfg.WEBUI_PORT,
    modelsDir: paths.modelsDir(),
    dataDir: paths.dataDir(),
    packaged: app.isPackaged,
    version: app.getVersion(),
    accel: loader.getAccel(),   // which ollama flavour was chosen + why
  }));
  ipcMain.handle('app:status', () => supervisor.snapshot());
  ipcMain.handle('app:logs', () => supervisor.recentLogs());
  ipcMain.handle('svc:startAll', () => { supervisor.startAll(); return true; });
  ipcMain.handle('svc:stopAll', () => { supervisor.stopAll(); return true; });
  ipcMain.handle('svc:restart', (_e, id) => {
    const s = supervisor.services().find((x) => x.id === id);
    if (s) s.restart();
    return true;
  });

  // --- llmfit model browser (proxied to `llmfit serve`, started by the launcher).
  // Proxying in main avoids renderer CORS/CSP and keeps the port server-side.
  const lfBase = () => process.env.PLANAI_LLMFIT_URL;
  const lfGet = async (p) => {
    const b = lfBase(); if (!b) throw new Error('model browser unavailable (llmfit not running)');
    const r = await fetch(b + p); if (!r.ok) throw new Error(`llmfit HTTP ${r.status}`);
    return r.json();
  };
  const lfPost = async (p, body) => {
    const b = lfBase(); if (!b) throw new Error('model browser unavailable (llmfit not running)');
    const r = await fetch(b + p, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
    if (!r.ok) throw new Error(`llmfit HTTP ${r.status}: ${await r.text().catch(() => '')}`);
    return r.json();
  };
  ipcMain.handle('llmfit:available', () => !!lfBase());
  ipcMain.handle('llmfit:models', (_e, q = {}) => {
    const params = new URLSearchParams({ limit: String(q.limit || 12), use_case: q.useCase || 'general' });
    if (q.minFit) params.set('min_fit', q.minFit);
    return lfGet(`/api/v1/models/top?${params}`);
  });
  ipcMain.handle('llmfit:installed', () => lfGet('/api/v1/installed'));
  ipcMain.handle('llmfit:download', (_e, model) => lfPost('/api/v1/download', { model, runtime: 'ollama' }));
  ipcMain.handle('llmfit:downloadStatus', (_e, id) => lfGet(`/api/v1/download/${encodeURIComponent(id)}/status`));
}

app.whenReady().then(() => {
  // first-launch: extract the components this machine needs (sets PLANAI_RESOURCES)
  try { loader.prepare((m) => console.log('[loader]', m)); }
  catch (e) { console.error('[loader] failed:', e.message); }
  supervisor = new Supervisor();
  createWindow();
  wireIpc();
  supervisor.startAll();

  app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) createWindow(); });
});

let didShutdown = false;
function shutdown() {
  if (didShutdown) return; didShutdown = true;
  if (supervisor) supervisor.stopAll();
  // release any squashfuse/hdiutil mounts AFTER the children that read them stop
  try { loader.unmountAll(); } catch (e) { console.error('[loader] unmount:', e.message); }
}
app.on('before-quit', shutdown);
app.on('window-all-closed', () => { shutdown(); if (process.platform !== 'darwin') app.quit(); });
