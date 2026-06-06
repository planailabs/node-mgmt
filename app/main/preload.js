'use strict';
const { contextBridge, ipcRenderer } = require('electron');

// Minimal, explicit bridge — the renderer never touches Node directly.
contextBridge.exposeInMainWorld('planai', {
  // info + initial state
  getInfo: () => ipcRenderer.invoke('app:info'),
  getStatus: () => ipcRenderer.invoke('app:status'),
  getLogs: () => ipcRenderer.invoke('app:logs'),

  // control
  start: () => ipcRenderer.invoke('svc:startAll'),
  stop: () => ipcRenderer.invoke('svc:stopAll'),
  restart: (id) => ipcRenderer.invoke('svc:restart', id),

  // events
  onStatus: (cb) => ipcRenderer.on('svc:status', (_e, p) => cb(p)),
  onLog: (cb) => ipcRenderer.on('svc:log', (_e, p) => cb(p)),
});
