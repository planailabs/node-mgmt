'use strict';
// Build the environment + ports for the supervised child processes.
// Everything here enforces the offline-kiosk contract: no auth, no network
// fetches at runtime (embedding model + nltk data are pre-bundled).
const path = require('path');
const paths = require('./paths');

const OLLAMA_HOST = '127.0.0.1';
const OLLAMA_PORT = 11434;
const WEBUI_HOST = '127.0.0.1';
const WEBUI_PORT = 8080;

function ollamaEnv() {
  return {
    ...process.env,
    OLLAMA_HOST: `${OLLAMA_HOST}:${OLLAMA_PORT}`,
    OLLAMA_MODELS: paths.modelsDir(),
    // keep the model store self-contained and quiet
    OLLAMA_KEEP_ALIVE: process.env.OLLAMA_KEEP_ALIVE || '5m',
  };
}

function webuiEnv() {
  const assets = paths.owAssets();
  return {
    ...process.env,
    HOST: WEBUI_HOST,
    PORT: String(WEBUI_PORT),
    // talk to our local ollama
    OLLAMA_BASE_URL: `http://${OLLAMA_HOST}:${OLLAMA_PORT}`,
    // kiosk: straight into chat, no login
    WEBUI_AUTH: 'False',
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
  };
}

module.exports = {
  OLLAMA_HOST, OLLAMA_PORT, WEBUI_HOST, WEBUI_PORT,
  ollamaEnv, webuiEnv,
  ollamaHealthUrl: `http://${OLLAMA_HOST}:${OLLAMA_PORT}/api/version`,
  webuiHealthUrl: `http://${WEBUI_HOST}:${WEBUI_PORT}/health`,
  webuiUrl: `http://${WEBUI_HOST}:${WEBUI_PORT}`,
};
