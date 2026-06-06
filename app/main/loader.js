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

// the ollama flavour for this machine
function ollamaArchive(comp) {
  const list = fs.readdirSync(comp);
  const has = (n) => list.includes(`${n}.tar.gz`) ? `${n}.tar.gz` : null;
  if (process.platform === 'linux') {
    if (fs.existsSync('/dev/kfd') && has('ollama-linux-amd64-rocm')) return has('ollama-linux-amd64-rocm');
    return process.arch === 'arm64' ? has('ollama-linux-arm64') : has('ollama-linux-amd64');
  }
  if (process.platform === 'darwin') return has('ollama-darwin');
  if (process.platform === 'win32') return has('ollama-windows-amd64');
  return null;
}

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
  const ol = ollamaArchive(comp);
  if (!rt) throw new Error('no runtime component in ' + comp);
  if (!ol) throw new Error('no ollama flavour for ' + process.platform + '/' + process.arch);

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

module.exports = { prepare, componentsDir };
