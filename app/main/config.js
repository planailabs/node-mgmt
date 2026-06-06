'use strict';
// Build the environment + ports for the supervised child processes.
// Everything here enforces the offline-kiosk contract: no auth, no network
// fetches at runtime (embedding model + nltk data are pre-bundled).
const path = require('path');
const fs = require('fs');
const crypto = require('crypto');
const paths = require('./paths');

// Get-or-create a persistent secret stored on the USB (DATA_DIR). Persisting it
// keeps Open-WebUI's encrypted fields decryptable across runs.
function persistentSecret(name) {
  const f = path.join(paths.dataDir(), `.${name}`);
  try {
    return fs.readFileSync(f, 'utf8').trim();
  } catch {
    const v = crypto.randomBytes(32).toString('hex');
    fs.writeFileSync(f, v, { mode: 0o600 });
    return v;
  }
}

const OLLAMA_HOST = '127.0.0.1';
const OLLAMA_PORT = 11434;
const WEBUI_HOST = '127.0.0.1';
const WEBUI_PORT = 8080;

// On NixOS dev runs, run-nixos.sh passes nix libs for the (foreign) child
// binaries via PLANAI_CHILD_LD_LIBRARY_PATH. Applied per child only — never to
// electron itself. No-op for the standalone product runtime.
function childLd(env) {
  const extra = process.env.PLANAI_CHILD_LD_LIBRARY_PATH;
  if (extra) env.LD_LIBRARY_PATH = extra + (env.LD_LIBRARY_PATH ? `:${env.LD_LIBRARY_PATH}` : '');
  return env;
}

function ollamaEnv() {
  return childLd({
    ...process.env,
    OLLAMA_HOST: `${OLLAMA_HOST}:${OLLAMA_PORT}`,
    OLLAMA_MODELS: paths.modelsDir(),
    // keep the model store self-contained and quiet
    OLLAMA_KEEP_ALIVE: process.env.OLLAMA_KEEP_ALIVE || '5m',
  });
}

function webuiEnv() {
  const assets = paths.owAssets();
  const frontend = paths.owFrontendDir();
  return childLd({
    ...process.env,
    // installed wheel serves the SPA from open_webui/frontend (env.py default is wrong)
    ...(frontend ? { FRONTEND_BUILD_DIR: frontend } : {}),
    HOST: WEBUI_HOST,
    PORT: String(WEBUI_PORT),
    // talk to our local ollama
    OLLAMA_BASE_URL: `http://${OLLAMA_HOST}:${OLLAMA_PORT}`,
    // kiosk: straight into chat, no login
    WEBUI_AUTH: 'False',
    // persistent secrets (required by OW 0.9.6) — stored on the USB
    WEBUI_SECRET_KEY: process.env.WEBUI_SECRET_KEY || persistentSecret('secret-key'),
    OAUTH_SESSION_TOKEN_ENCRYPTION_KEY:
      process.env.OAUTH_SESSION_TOKEN_ENCRYPTION_KEY || persistentSecret('oauth-key'),
    // data on the USB
    DATA_DIR: paths.dataDir(),
    // fully offline: use pre-bundled embedding model + nltk data
    HF_HUB_OFFLINE: '1',
    TRANSFORMERS_OFFLINE: '1',
    HF_HOME: path.join(assets, 'hf'),
    SENTENCE_TRANSFORMERS_HOME: path.join(assets, 'hf'),
    NLTK_DATA: path.join(assets, 'nltk'),
    // don't try to phone home for version checks / telemetry
    SCARF_NO_ANALYTICS: 'true',
    DO_NOT_TRACK: 'true',
    ANONYMIZED_TELEMETRY: 'False',
  });
}

module.exports = {
  OLLAMA_HOST, OLLAMA_PORT, WEBUI_HOST, WEBUI_PORT,
  ollamaEnv, webuiEnv,
  ollamaHealthUrl: `http://${OLLAMA_HOST}:${OLLAMA_PORT}/api/version`,
  webuiHealthUrl: `http://${WEBUI_HOST}:${WEBUI_PORT}/health`,
  webuiUrl: `http://${WEBUI_HOST}:${WEBUI_PORT}`,
};
