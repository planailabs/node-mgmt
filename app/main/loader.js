'use strict';
// Cross-platform lazy component loader (runs in the Electron main process on
// EVERY platform). Artifacts ship compressed component archives under
// resources/components/; on first launch we extract ONLY what this machine
// needs — the OS runtime, the offline assets, and the single ollama flavour
// matching the CPU arch (or ROCm if an AMD GPU is present) — into a writable
// cache. Subsequent launches skip extraction. paths.js then reads
// PLANAI_RESOURCES (the extracted tree) so the rest of the app is unchanged.
const fs = require('fs');
const path = require('path');
const os = require('os');
const { execFileSync } = require('child_process');
const tar = require('tar'); // pure-JS, streams .tar.gz (no native/zstd dep)
const { app } = require('electron');

function componentsDir() {
  const cands = [
    process.env.PLANAI_COMPONENTS,
    app.isPackaged ? path.join(process.resourcesPath, 'components') : null,
    path.join(__dirname, '..', '..', 'dist', 'components'), // dev
    path.join(path.dirname(process.execPath), 'components'),
  ].filter(Boolean);
  return cands.find((c) => fs.existsSync(path.join(c, 'manifest.json')) || fs.existsSync(c)) || null;
}

function cacheRoot() {
  if (process.env.PLANAI_CACHE) return process.env.PLANAI_CACHE;
  const base = (() => { try { return app.getPath('cache'); } catch { return os.tmpdir(); } })();
  return path.join(base, 'plan-ai');
}

// the single runtime archive shipped in this bundle (one per OS)
function runtimeArchive(comp) {
  return fs.readdirSync(comp).find((f) => /^runtime-.*\.tar\.gz$/.test(f)) || null;
}

// Is a shared library findable on this system (common dirs + ldconfig cache)?
function libPresent(names) {
  const dirs = ['/opt/rocm/lib', '/usr/lib', '/usr/lib64', '/usr/lib/x86_64-linux-gnu', '/lib/x86_64-linux-gnu'];
  for (const n of names) for (const d of dirs) if (fs.existsSync(path.join(d, n))) return true;
  try {
    const cache = execFileSync('ldconfig', ['-p'], { encoding: 'utf8' });
    if (names.some((n) => cache.includes(n))) return true;
  } catch { /* no ldconfig (e.g. NixOS) — rely on the dir checks */ }
  return false;
}

// Choose the ollama flavour by checking the actual hardware/runtime, and record
// WHY each candidate was or wasn't picked (surfaced in the dashboard overview).
// rocm is only selected when its archive is bundled AND /dev/kfd exists AND the
// ROCm runtime libs are installed — otherwise that build crashes on launch.
function detectOllama(comp) {
  const list = fs.readdirSync(comp);
  const has = (n) => list.includes(`${n}.tar.gz`);
  const checks = [];
  const add = (flavour, bundled, usable, why) => checks.push({ flavour, bundled, usable, why });

  let chosen = null;
  if (process.platform === 'linux' && process.arch !== 'arm64') {
    // ROCm (AMD GPU) candidate
    const rocmBundled = has('ollama-linux-amd64-rocm');
    const kfd = fs.existsSync('/dev/kfd');
    const rocmLib = libPresent(['libamdhip64.so', 'libamdhip64.so.6', 'librocm-core.so']);
    const rocmUsable = rocmBundled && kfd && rocmLib;
    add('linux-amd64-rocm', rocmBundled, rocmUsable,
      !rocmBundled ? 'not bundled in this build (CPU build)'
      : !kfd ? 'no AMD GPU (/dev/kfd absent)'
      : !rocmLib ? 'ROCm runtime not installed (libamdhip64 not found)'
      : 'AMD GPU + ROCm runtime detected');
    if (rocmUsable) chosen = 'ollama-linux-amd64-rocm';
    // CPU/CUDA amd64 (default; ollama uses CUDA at runtime if libcuda is present)
    if (!chosen) {
      const cuda = libPresent(['libcuda.so', 'libcuda.so.1']);
      add('linux-amd64', has('ollama-linux-amd64'), has('ollama-linux-amd64'),
        cuda ? 'NVIDIA driver present — ollama will use CUDA, else CPU' : 'CPU (no NVIDIA/AMD GPU runtime detected)');
      if (has('ollama-linux-amd64')) chosen = 'ollama-linux-amd64';
    }
  } else if (process.platform === 'linux') {
    add('linux-arm64', has('ollama-linux-arm64'), has('ollama-linux-arm64'), 'arm64 CPU');
    if (has('ollama-linux-arm64')) chosen = 'ollama-linux-arm64';
  } else if (process.platform === 'darwin') {
    add('darwin', has('ollama-darwin'), has('ollama-darwin'), 'macOS universal (Metal)');
    if (has('ollama-darwin')) chosen = 'ollama-darwin';
  } else if (process.platform === 'win32') {
    add('windows-amd64', has('ollama-windows-amd64'), has('ollama-windows-amd64'), 'Windows x64');
    if (has('ollama-windows-amd64')) chosen = 'ollama-windows-amd64';
  }

  const selected = checks.find((c) => `ollama-${c.flavour}.tar.gz` === chosen);
  return {
    archive: chosen,
    flavour: selected ? selected.flavour : null,
    reason: selected ? selected.why : 'no ollama flavour available for this machine',
    checks,
  };
}

let lastAccel = null;
function getAccel() { return lastAccel; }

function extractOnce(archive, dest, marker) {
  if (fs.existsSync(marker)) return;
  fs.mkdirSync(dest, { recursive: true });
  // tar.x is synchronous with { sync: true } and handles gzip transparently
  tar.x({ file: archive, cwd: dest, sync: true });
  fs.writeFileSync(marker, path.basename(archive));
}

// Extract the needed components into <cache>/dist and point PLANAI_RESOURCES there.
// No-op (returns false) when there is no components/ dir (plain dev tree).
function prepare(log = () => {}) {
  const comp = componentsDir();
  if (!comp || !fs.existsSync(path.join(comp, (runtimeArchive(comp) || '')))) return false;

  const root = path.join(cacheRoot(), 'root');
  const dist = path.join(root, 'dist');
  fs.mkdirSync(dist, { recursive: true });

  const rt = runtimeArchive(comp);
  const accel = detectOllama(comp);
  lastAccel = accel;
  const ol = accel.archive;
  if (!rt) throw new Error('no runtime component in ' + comp);
  if (!ol) throw new Error('no ollama flavour for ' + process.platform + '/' + process.arch);
  log(`ollama flavour: ${accel.flavour} — ${accel.reason}`);

  log(`extracting runtime ${rt}`);
  extractOnce(path.join(comp, rt), path.join(dist, 'runtime'), path.join(root, `.runtime.${rt}.done`));
  if (fs.existsSync(path.join(comp, 'ow-assets.tar.gz'))) {
    log('extracting ow-assets');
    extractOnce(path.join(comp, 'ow-assets.tar.gz'), path.join(dist, 'ow-assets'), path.join(root, '.ow-assets.done'));
  }
  log(`extracting ollama ${ol}`);
  const ollamaDest = path.join(dist, 'ollama');
  extractOnce(path.join(comp, ol), ollamaDest, path.join(root, `.ollama.${ol}.done`));

  // NixOS: the extracted ollama is a generic ELF the bare nix-ld stub can't run.
  // The NixOS launcher sets PLANAI_NIX_LD/PLANAI_NIX_LD_LIBRARY_PATH so we repoint
  // its interpreter (once) at the nix loader. No-op on every other platform.
  if (process.env.PLANAI_NIX_LD) {
    const marker = path.join(root, `.ollama.${ol}.patched`);
    if (!fs.existsSync(marker)) {
      const bin = [path.join(ollamaDest, 'bin', 'ollama'), path.join(ollamaDest, 'ollama')]
        .find((p) => fs.existsSync(p));
      if (bin) {
        try {
          execFileSync('patchelf', ['--set-interpreter', process.env.PLANAI_NIX_LD, bin]);
          if (process.env.PLANAI_NIX_LD_LIBRARY_PATH)
            execFileSync('patchelf', ['--add-rpath', process.env.PLANAI_NIX_LD_LIBRARY_PATH, bin]);
          fs.writeFileSync(marker, bin);
        } catch (e) { log('patchelf ollama failed: ' + e.message); }
      }
    }
  }

  process.env.PLANAI_RESOURCES = dist;
  return true;
}

module.exports = { prepare, componentsDir, getAccel };
