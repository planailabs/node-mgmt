'use strict';
// Resolve every path the app needs, both in dev (`electron .` from the repo)
// and when packaged (AppImage/.exe/.app running off a USB stick).
const path = require('path');
const fs = require('fs');
const { app } = require('electron');

const PLATFORM = process.platform; // 'linux' | 'win32' | 'darwin'
const ARCH = process.arch; // 'x64' | 'arm64'

// Where bundled, read-only resources live (ollama binary, python runtime, assets).
function resourcesRoot() {
  if (app.isPackaged) return process.resourcesPath;
  // dev: the build scripts populate dist/
  return path.join(__dirname, '..', '..', 'dist');
}

// Writable USB-side root that holds models/ and data/ next to the binary.
// AppImage exposes APPIMAGE (the .AppImage path); fall back to the exe dir.
function portableRoot() {
  if (!app.isPackaged) return path.join(__dirname, '..', '.run');
  if (process.env.APPIMAGE) return path.dirname(process.env.APPIMAGE);
  if (PLATFORM === 'darwin') {
    // .../plan.ai.app/Contents/MacOS/plan.ai -> dir containing the .app
    return path.resolve(path.dirname(process.execPath), '..', '..', '..');
  }
  return path.dirname(process.execPath);
}

function ensureDir(p) {
  fs.mkdirSync(p, { recursive: true });
  return p;
}

// Pick the ollama binary matching the running OS/arch from resources/ollama/.
function ollamaBinary() {
  const dir = path.join(resourcesRoot(), 'ollama');
  const name = PLATFORM === 'win32' ? 'ollama.exe' : 'ollama';
  // bundle.sh extracts the chosen flavour to resources/ollama/<bin>;
  // also tolerate a bin/ subdir (some archives nest under bin/).
  for (const cand of [path.join(dir, name), path.join(dir, 'bin', name)]) {
    if (fs.existsSync(cand)) return cand;
  }
  return path.join(dir, name); // report the expected path even if missing
}

// The python interpreter inside the relocatable venv.
function venvPython() {
  const venv = path.join(resourcesRoot(), 'runtime', 'venv');
  return PLATFORM === 'win32'
    ? path.join(venv, 'Scripts', 'python.exe')
    : path.join(venv, 'bin', 'python');
}

// Open-WebUI's installed frontend dir. The wheel force-includes the built SPA at
// open_webui/frontend, but env.py defaults FRONTEND_BUILD_DIR to BASE_DIR/build
// (wrong for an installed wheel), so we resolve + pass it explicitly.
function owFrontendDir() {
  const venv = path.join(resourcesRoot(), 'runtime', 'venv');
  const candidates = [path.join(venv, 'Lib', 'site-packages', 'open_webui', 'frontend')];
  const libdir = path.join(venv, 'lib');
  try {
    for (const d of fs.readdirSync(libdir)) {
      candidates.push(path.join(libdir, d, 'site-packages', 'open_webui', 'frontend'));
    }
  } catch { /* no lib/ (e.g. windows) */ }
  for (const c of candidates) {
    if (fs.existsSync(path.join(c, 'index.html'))) return c;
  }
  return '';
}

const paths = {
  PLATFORM,
  ARCH,
  resourcesRoot,
  portableRoot,
  ollamaBinary,
  venvPython,
  owFrontendDir,
  modelsDir: () => ensureDir(path.join(portableRoot(), 'models')),
  dataDir: () => ensureDir(path.join(portableRoot(), 'data')),
  owAssets: () => path.join(resourcesRoot(), 'ow-assets'),
  rendererIndex: () => path.join(__dirname, '..', 'renderer', 'index.html'),
  preload: () => path.join(__dirname, 'preload.js'),
};

module.exports = paths;
