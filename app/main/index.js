'use strict';
// Thin Electron shell (phase 5): the rust launcher prepares the runtime, runs the
// control plane (ollama + open-webui supervisor) and serves the Dioxus SPA over
// localhost. Electron just shows that URL — no node supervisor/loader/renderer.
const { app, BrowserWindow, shell } = require('electron');

const UI_URL = process.env.PLANAI_UI_URL || '';

function createWindow() {
  const win = new BrowserWindow({
    width: 1200,
    height: 820,
    minWidth: 900,
    minHeight: 600,
    backgroundColor: '#0f0e0c',
    title: 'plan.ai',
    show: false,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  win.removeMenu();

  // External links (e.g. the dashboard's "Report issue" button → window.open)
  // open in the user's default browser rather than a bare Electron popup.
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:\/\//i.test(url)) shell.openExternal(url);
    return { action: 'deny' };
  });

  if (UI_URL) {
    win.loadURL(UI_URL);
  } else {
    win.loadURL(
      'data:text/html,' +
        encodeURIComponent(
          '<body style="font-family:system-ui;background:#0f0e0c;color:#eee;' +
            'display:grid;place-items:center;height:100vh;margin:0">' +
            '<p>plan.ai UI server is unavailable (PLANAI_UI_URL unset).</p></body>'
        )
    );
  }
  win.once('ready-to-show', () => {
    win.show();
    // Signal the launcher that the window is up so it closes the native splash
    // spinner (it shows during the pre-Electron runtime mount). Best-effort.
    if (UI_URL) {
      try {
        const { request } = require('http');
        const req = request(new URL('/api/ready', UI_URL), { method: 'POST' });
        req.on('error', () => {});
        req.end();
      } catch (_) {
        /* ignore — the launcher has a timeout fallback */
      }
    }
  });

  // CI/dev smoke: capture the dashboard then exit. PLANAI_CAPTURE=/dash.png.
  if (process.env.PLANAI_CAPTURE) {
    const fs = require('fs');
    win.webContents.once('did-finish-load', () => {
      setTimeout(async () => {
        try {
          fs.writeFileSync(process.env.PLANAI_CAPTURE, (await win.webContents.capturePage()).toPNG());
        } catch (e) {
          console.error('capture failed', e);
        }
        app.quit();
      }, Number(process.env.PLANAI_CAPTURE_DELAY || 2500));
    });
  }
}

app.whenReady().then(() => {
  createWindow();
  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});
