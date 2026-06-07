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
function setTabActive(id, active) {
  const b = $(id);
  if (b.disabled) return;
  b.classList.toggle('btn-secondary', active);
  b.classList.toggle('btn-ghost', !active);
}
function showView(which) {
  ['dashboard', 'models', 'webui'].forEach((v) =>
    $('view-' + v).classList.toggle('hidden', v !== which));
  setTabActive('tab-dashboard', which === 'dashboard');
  setTabActive('tab-models', which === 'models');
  setTabActive('tab-webui', which === 'webui');
  if (which === 'webui') loadWebui();
  if (which === 'models') loadModels();
}

function loadWebui() {
  if (webuiLoaded || !info) return;
  const wv = $('webui');
  wv.src = info.webuiUrl;
  webuiLoaded = true;
}

// ---- models (llmfit) ------------------------------------------------------
let modelsLoaded = false;

function renderHardware(sys) {
  if (!sys) return;
  const gpu = sys.gpu_name
    ? `${sys.gpu_name} · ${sys.gpu_vram_gb ?? '?'} GB VRAM · ${sys.backend}`
    : `CPU only · ${sys.backend}`;
  const ram = typeof sys.total_ram_gb === 'number' ? sys.total_ram_gb.toFixed(1) : sys.total_ram_gb;
  $('m-hw').textContent = `${gpu}  —  ${ram} GB RAM  —  ${sys.cpu_name}`;
}

function fitPill(level) {
  return { perfect: 'pill-ok', good: 'pill-ok', marginal: 'pill-warn', tight: 'pill-warn' }[level] || 'pill-muted';
}

async function loadModels(force) {
  if (modelsLoaded && !force) return;
  modelsLoaded = true;
  const status = $('m-status');
  status.textContent = 'loading…';
  try {
    const data = await planai.llmfit.models({
      useCase: $('m-usecase').value,
      minFit: $('m-minfit').value,
      limit: 12,
    });
    renderHardware(data.system);
    renderModels(data.models || []);
    status.textContent = `${data.returned_models}/${data.total_models} shown`;
  } catch (e) {
    status.textContent = e.message;
    $('m-list').innerHTML = `<div class="card-pad td-muted text-sm">${e.message}</div>`;
  }
  loadInstalled();
}

function renderModels(models) {
  const host = $('m-list');
  host.innerHTML = '';
  if (!models.length) {
    host.innerHTML = '<div class="card-pad td-muted text-sm">no compatible models for this filter</div>';
    return;
  }
  for (const m of models) {
    const row = document.createElement('div');
    row.className = 'card-pad flex items-center justify-between gap-3';
    const params = m.parameter_count || (m.params_b ? `${m.params_b}B` : '');
    const tps = m.estimated_tps ? `~${Math.round(m.estimated_tps)} tok/s` : '';
    const meta = [params, m.best_quant, m.run_mode_label, tps].filter(Boolean).join(' · ');
    const key = encodeURIComponent(m.name);
    row.innerHTML = `
      <div class="min-w-0">
        <div class="font-mono text-fg-strong truncate">${m.name}</div>
        <div class="help-xs td-muted">${meta}</div>
      </div>
      <div class="flex items-center gap-2 shrink-0">
        <span class="pill ${fitPill(m.fit_level)}">${m.fit_label || m.fit_level || ''}</span>
        <button class="btn btn-xs btn-accent" data-dl="${key}">Download</button>
        <span class="help-xs td-muted" data-prog="${key}"></span>
      </div>`;
    host.appendChild(row);
  }
  host.querySelectorAll('[data-dl]').forEach((b) =>
    b.addEventListener('click', () => downloadModel(decodeURIComponent(b.dataset.dl), b)));
}

async function downloadModel(name, btn) {
  const prog = document.querySelector(`[data-prog="${encodeURIComponent(name)}"]`);
  btn.disabled = true;
  if (prog) prog.textContent = 'starting…';
  try {
    const { id } = await planai.llmfit.download(name);
    const poll = async () => {
      try {
        const s = await planai.llmfit.downloadStatus(id);
        const pct = s.progress_pct ? `${Math.round(s.progress_pct)}%` : '';
        if (prog) prog.textContent = `${s.status} ${pct} ${s.message || ''}`.replace(/\s+/g, ' ').trim();
        if (s.status === 'pulling' || s.status === 'starting') {
          setTimeout(poll, 1200);
        } else {
          btn.disabled = s.status === 'complete' || s.status === 'completed' || s.status === 'success';
          loadInstalled();
        }
      } catch (e) { if (prog) prog.textContent = e.message; btn.disabled = false; }
    };
    poll();
  } catch (e) { if (prog) prog.textContent = e.message; btn.disabled = false; }
}

async function loadInstalled() {
  try {
    const d = await planai.llmfit.installed();
    const list = Array.isArray(d) ? d : (d.installed || d.models || []);
    const names = list.map((x) => (typeof x === 'string' ? x : x.name || x.model)).filter(Boolean);
    $('m-installed').textContent = names.length ? names.join('   ·   ') : 'none yet';
  } catch (e) { $('m-installed').textContent = e.message; }
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

  // acceleration / ollama flavour decision + why (from the loader)
  const ac = info.accel;
  if (ac && ac.flavour) {
    $('f-accel').textContent = ac.flavour;
    $('f-accel-why').textContent = `— ${ac.reason}`;
    if (Array.isArray(ac.checks)) {
      $('f-accel-checks').innerHTML = ac.checks.map((c) => {
        const mark = c.usable ? '✓' : '·';
        const sel = c.flavour === ac.flavour ? ' (selected)' : '';
        return `<li>${mark} ${c.flavour}: ${c.why}${sel}</li>`;
      }).join('');
    }
  } else {
    $('f-accel').textContent = 'bundled runtime';
    $('f-accel-why').textContent = ac ? `— ${ac.reason}` : '';
  }

  // Models tab — enabled only when the launcher started llmfit serve.
  if (ac && ac.llmfitUrl) {
    $('tab-models').disabled = false;
    renderHardware(ac.gpu);
  }

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
  $('tab-models').addEventListener('click', () => showView('models'));
  $('tab-webui').addEventListener('click', () => showView('webui'));
  $('m-refresh').addEventListener('click', () => loadModels(true));
  $('m-usecase').addEventListener('change', () => loadModels(true));
  $('m-minfit').addEventListener('change', () => loadModels(true));
  $('open-webui-cta').addEventListener('click', () => showView('webui'));
  $('clear-logs').addEventListener('click', () => { $('logs').textContent = ''; });
  $('theme').addEventListener('click', () =>
    document.documentElement.classList.toggle('dark'));
}

init();
