'use strict';
// Spawn, health-check, and restart the two services (ollama + open-webui).
// Emits events the renderer consumes: 'status' (per service) and 'log'.
const { EventEmitter } = require('events');
const { spawn } = require('child_process');
const fs = require('fs');
const http = require('http');
const paths = require('./paths');
const cfg = require('./config');

const MAX_RESTARTS = 5;
const RESTART_BASE_MS = 1000;

function httpOk(url, timeoutMs = 1500) {
  return new Promise((resolve) => {
    const req = http.get(url, { timeout: timeoutMs }, (res) => {
      res.resume();
      resolve(res.statusCode >= 200 && res.statusCode < 500);
    });
    req.on('timeout', () => { req.destroy(); resolve(false); });
    req.on('error', () => resolve(false));
  });
}

class Service extends EventEmitter {
  constructor({ id, name, bin, args, env, healthUrl }) {
    super();
    Object.assign(this, { id, name, bin, args, env, healthUrl });
    this.proc = null;
    this.state = 'stopped'; // stopped | starting | ready | error
    this.restarts = 0;
    this.stopping = false;
    this._healthTimer = null;
  }

  setState(state, detail) {
    this.state = state;
    this.emit('status', { id: this.id, name: this.name, state, detail: detail || '' });
  }

  log(line) { this.emit('log', { id: this.id, line: String(line).replace(/\s+$/, '') }); }

  start() {
    if (this.proc) return;
    if (!fs.existsSync(this.bin)) {
      this.setState('error', `binary not found: ${this.bin}`);
      this.log(`[${this.id}] binary not found: ${this.bin} (build/bundle step not run?)`);
      return;
    }
    this.stopping = false;
    this.setState('starting');
    this.log(`[${this.id}] spawn ${this.bin} ${this.args.join(' ')}`);
    const proc = spawn(this.bin, this.args, { env: this.env, stdio: ['ignore', 'pipe', 'pipe'] });
    this.proc = proc;
    proc.stdout.on('data', (d) => this.log(d));
    proc.stderr.on('data', (d) => this.log(d));
    proc.on('exit', (code, sig) => {
      this.proc = null;
      this._stopHealth();
      if (this.stopping) { this.setState('stopped'); return; }
      this.setState('error', `exited code=${code} sig=${sig || ''}`);
      if (this.restarts < MAX_RESTARTS) {
        const delay = RESTART_BASE_MS * 2 ** this.restarts;
        this.restarts += 1;
        this.log(`[${this.id}] restarting in ${delay}ms (attempt ${this.restarts}/${MAX_RESTARTS})`);
        setTimeout(() => this.start(), delay);
      } else {
        this.log(`[${this.id}] gave up after ${MAX_RESTARTS} restarts`);
      }
    });
    this._pollHealth();
  }

  _pollHealth() {
    this._stopHealth();
    this._healthTimer = setInterval(async () => {
      if (!this.proc) return;
      if (await httpOk(this.healthUrl)) {
        if (this.state !== 'ready') { this.restarts = 0; this.setState('ready'); }
      }
    }, 1000);
  }

  _stopHealth() { if (this._healthTimer) { clearInterval(this._healthTimer); this._healthTimer = null; } }

  stop() {
    this.stopping = true;
    this._stopHealth();
    if (this.proc) {
      this.proc.kill('SIGTERM');
      // hard kill if it lingers
      setTimeout(() => { if (this.proc) this.proc.kill('SIGKILL'); }, 4000);
    } else {
      this.setState('stopped');
    }
  }

  async restart() { this.restarts = 0; this.stop(); await new Promise((r) => setTimeout(r, 500)); this.start(); }
}

class Supervisor extends EventEmitter {
  constructor() {
    super();
    this.logBuffer = []; // recent lines, replayed to the renderer on init
    this.ollama = new Service({
      id: 'ollama', name: 'Ollama',
      bin: paths.ollamaBinary(), args: ['serve'],
      env: cfg.ollamaEnv(), healthUrl: cfg.ollamaHealthUrl,
    });
    this.webui = new Service({
      id: 'webui', name: 'Open-WebUI',
      bin: paths.venvPython(),
      args: ['-m', 'uvicorn', 'open_webui.main:app', '--host', cfg.WEBUI_HOST, '--port', String(cfg.WEBUI_PORT)],
      env: cfg.webuiEnv(), healthUrl: cfg.webuiHealthUrl,
    });
    for (const s of [this.ollama, this.webui]) {
      s.on('status', (e) => this.emit('status', e));
      s.on('log', (e) => {
        this.logBuffer.push(e);
        if (this.logBuffer.length > 500) this.logBuffer.shift();
        this.emit('log', e);
      });
    }
  }

  recentLogs() { return this.logBuffer.slice(); }
  services() { return [this.ollama, this.webui]; }
  startAll() { this.ollama.start(); this.webui.start(); }
  stopAll() { this.webui.stop(); this.ollama.stop(); }
  snapshot() {
    return this.services().map((s) => ({ id: s.id, name: s.name, state: s.state }));
  }
}

module.exports = { Supervisor };
