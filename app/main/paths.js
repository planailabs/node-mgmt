'use strict';
// Resolve every path the app needs, both in dev (`electron .` from the repo)
// and when packaged (AppImage/.exe/.app running off a USB stick).
const path = require('path');
const fs = require('fs');
const { app } = require('electron');

const PLATFORM = process.platform; // 'linux' | 'win32' | 'darwin'
const ARCH = process.arch; // 'x64' | 'arm64'

// Where the extracted resources live (ollama binary, python runtime, assets).
// The loader (main/loader.js) sets PLANAI_RESOURCES to the per-machine cache it
// extracted the right components into; fall back to the packaged resources dir
// (legacy/no-components) or the dev dist/ tree.
function resourcesRoot() {
  if (process.env.PLANAI_RESOURCES) return process.env.PLANAI_RESOURCES;
  if (app.isPackaged) return process.resourcesPath;
  return path.join(__dirname, '..', '..', 'dist');
}

// Writable USB-side root that holds models/ and data/ next to the binary.
// AppImage exposes APPIMAGE (the .AppImage path); fall back to the exe dir.
// PLANAI_PORTABLE_ROOT overrides everything (used by the FAT32 USB image test).
function portableRoot() {
  if (process.env.PLANAI_PORTABLE_ROOT) return process.env.PLANAI_PORTABLE_ROOT;
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

function runtimeRoot() { return path.join(resourcesRoot(), 'runtime'); }

// The python interpreter — works for a host venv (runtime/venv) or a cross
// python-build-standalone tree (runtime/python).
function venvPython() {
  const rt = runtimeRoot();
  const cands = [];
  if (PLATFORM === 'win32') {
    cands.push(path.join(rt, 'venv', 'Scripts', 'python.exe'));
    cands.push(path.join(rt, 'python', 'python.exe'));
  } else {
    cands.push(path.join(rt, 'venv', 'bin', 'python'));
    for (const base of ['python', 'venv']) {
      cands.push(path.join(rt, base, 'bin', 'python3'));
      try {
        for (const f of fs.readdirSync(path.join(rt, base, 'bin'))) {
          if (/^python3(\.\d+)?$/.test(f)) cands.push(path.join(rt, base, 'bin', f));
        }
      } catch { /* missing */ }
    }
  }
  for (const c of cands) if (fs.existsSync(c)) return c;
  return cands[0];
}

// All site-packages roots across both runtime layouts.
function sitePackagesRoots() {
  const rt = runtimeRoot();
  const roots = [];
  for (const base of ['venv', 'python']) {
    roots.push(path.join(rt, base, 'Lib', 'site-packages')); // windows
    const lib = path.join(rt, base, 'lib');
    try {
      for (const d of fs.readdirSync(lib)) roots.push(path.join(lib, d, 'site-packages'));
    } catch { /* missing */ }
  }
  return roots;
}

// Open-WebUI's installed frontend dir. The wheel force-includes the built SPA at
// open_webui/frontend, but env.py defaults FRONTEND_BUILD_DIR to BASE_DIR/build
// (wrong for an installed wheel), so we resolve + pass it explicitly.
function owFrontendDir() {
  for (const sp of sitePackagesRoots()) {
    const c = path.join(sp, 'open_webui', 'frontend');
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
