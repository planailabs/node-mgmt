'use strict';
/* global planai */

// ---- state ----------------------------------------------------------------
const svc = {
  ollama: { id: 'ollama', name: 'Ollama', state: 'starting' },
  webui: { id: 'webui', name: 'Open-WebUI', state: 'starting' },
};
let info = null;
let webuiLoaded = false;

const STATE_PILL = {
  ready: 'pill-ok',
  starting: 'pill-warn',
  stopped: 'pill-muted',
  error: 'pill-bad',
};
const STATE_DOT = {
  ready: 'dot-ok',
  starting: 'dot-warn',
  stopped: 'dot-muted',
  error: 'dot-bad',
};
const STATE_LABEL = {
  ready: 'ready', starting: 'starting', stopped: 'stopped', error: 'error',
};

const $ = (id) => document.getElementById(id);

// ---- rendering ------------------------------------------------------------
function renderServices() {
  const host = $('services');
  host.innerHTML = '';
  for (const s of Object.values(svc)) {
    const card = document.createElement('div');
    card.className = 'card card-pad flex items-center justify-between';
    card.innerHTML = `
      <div class="flex items-center gap-3">
        <span class="dot ${STATE_DOT[s.state] || 'dot-muted'}"></span>
        <div>
          <div class="h-card">${s.name}</div>
          <div class="help-xs">${s.id === 'ollama' ? 'LLM runtime' : 'chat UI'}</div>
        </div>
      </div>
      <div class="flex items-center gap-2">
        <span class="pill ${STATE_PILL[s.state] || 'pill-muted'}">${STATE_LABEL[s.state] || s.state}</span>
        <button class="btn btn-xs btn-secondary" data-restart="${s.id}">restart</button>
      </div>`;
    host.appendChild(card);
  }
  host.querySelectorAll('[data-restart]').forEach((b) =>
    b.addEventListener('click', () => planai.restart(b.dataset.restart)));

  // WebUI tab/CTA become available only when open-webui is ready.
  const ready = svc.webui.state === 'ready';
  $('tab-webui').disabled = !ready;
  $('open-webui-cta').disabled = !ready;
  $('tab-webui').classList.toggle('btn-ghost', !ready);
  $('tab-webui').classList.toggle('btn-secondary', ready);
}

function appendLog({ id, line }) {
  const el = $('logs');
  const at = el.scrollTop + el.clientHeight >= el.scrollHeight - 4;
  el.textContent += `${line}\n`;
  // cap buffer
  if (el.textContent.length > 200000) el.textContent = el.textContent.slice(-150000);
  if (at) el.scrollTop = el.scrollHeight;
}

// ---- view switching -------------------------------------------------------
function showView(which) {
  const dash = which === 'dashboard';
  $('view-dashboard').classList.toggle('hidden', !dash);
  $('view-webui').classList.toggle('hidden', dash);
  $('tab-dashboard').classList.toggle('btn-secondary', dash);
  $('tab-dashboard').classList.toggle('btn-ghost', !dash);
  if (!dash) loadWebui();
}

function loadWebui() {
  if (webuiLoaded || !info) return;
  const wv = $('webui');
  wv.src = info.webuiUrl;
  webuiLoaded = true;
}

// ---- wiring ---------------------------------------------------------------
async function init() {
  info = await planai.getInfo();
  $('f-ollama-port').textContent = info.ollamaPort;
  $('f-webui-port').textContent = info.webuiPort;
  $('f-models').textContent = info.modelsDir;
  $('f-data').textContent = info.dataDir;
  $('f-models').title = info.modelsDir;
  $('f-data').title = info.dataDir;

  const snap = await planai.getStatus();
  snap.forEach((s) => { if (svc[s.id]) svc[s.id].state = s.state; });
  renderServices();

  // replay any logs emitted before the renderer was ready
  (await planai.getLogs()).forEach(appendLog);

  planai.onStatus((e) => {
    if (svc[e.id]) { svc[e.id].state = e.state; svc[e.id].detail = e.detail; }
    renderServices();
  });
  planai.onLog(appendLog);

  $('start').addEventListener('click', () => planai.start());
  $('stop').addEventListener('click', () => planai.stop());
  $('tab-dashboard').addEventListener('click', () => showView('dashboard'));
  $('tab-webui').addEventListener('click', () => showView('webui'));
  $('open-webui-cta').addEventListener('click', () => showView('webui'));
  $('clear-logs').addEventListener('click', () => { $('logs').textContent = ''; });
  $('theme').addEventListener('click', () =>
    document.documentElement.classList.toggle('dark'));
}

init();
