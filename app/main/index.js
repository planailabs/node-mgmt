'use strict';
const { app, BrowserWindow, ipcMain } = require('electron');
const paths = require('./paths');
const cfg = require('./config');
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

  // CI/dev smoke: capture the dashboard then exit (PLANAI_CAPTURE=/path.png).
  if (process.env.PLANAI_CAPTURE) {
    win.webContents.once('did-finish-load', () => {
      setTimeout(async () => {
        try {
          const img = await win.webContents.capturePage();
          require('fs').writeFileSync(process.env.PLANAI_CAPTURE, img.toPNG());
        } catch (e) { console.error('capture failed', e); }
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
}

app.whenReady().then(() => {
  supervisor = new Supervisor();
  createWindow();
  wireIpc();
  supervisor.startAll();

  app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) createWindow(); });
});

function shutdown() { if (supervisor) supervisor.stopAll(); }
app.on('before-quit', shutdown);
app.on('window-all-closed', () => { shutdown(); if (process.platform !== 'darwin') app.quit(); });
