'use strict';
// Cross-platform lazy component loader (runs in the Electron main process on
// EVERY platform). Artifacts ship per-OS component images under
// resources/components/; on first launch we PROVIDE only what this machine needs
// — the OS runtime, the offline assets, and the single ollama flavour matching
// the CPU arch (or ROCm if an AMD GPU is present):
//
//   linux/nixos : <name>.squashfs  → MOUNTED in place via bundled static
//                 squashfuse_ll (no 6 GB extraction); falls back to extracting
//                 with bundled static unsquashfs when FUSE is unavailable.
//   macOS       : <name>.dmg       → mounted via hdiutil (else .tar.gz extract).
//   windows     : <name>.tar.gz    → extracted (pure-JS tar).
//
// paths.js then reads PLANAI_RESOURCES (the provided tree) so the rest of the
// app is unchanged. Mounts are tracked and released on quit.
const fs = require('fs');
const path = require('path');
const os = require('os');
const { execFileSync } = require('child_process');
const tar = require('tar'); // pure-JS, streams .tar.gz (no native/zstd dep)
const { app } = require('electron');

// Roots NEXT TO the launcher on the USB, where the shared components/ and
// tools/ dirs live (so they are not embedded — and duplicated — in every app):
//   linux AppImage : the dir containing the .AppImage file (process.env.APPIMAGE)
//   macOS .app     : the dir containing plan.ai.app (execPath is .app/Contents/MacOS/plan.ai)
//   windows / nixos: the dir containing the launcher (execPath / dir)
function externalRoots() {
  const roots = [];
  if (process.env.APPIMAGE) roots.push(path.dirname(process.env.APPIMAGE));
  if (process.platform === 'darwin') roots.push(path.resolve(process.execPath, '..', '..', '..', '..'));
  const exeDir = path.dirname(process.execPath);
  roots.push(exeDir);                 // exe / launcher dir
  roots.push(path.dirname(exeDir));   // one up: win zip extracted into a subdir beside components/
  return roots;
}

function componentsDir() {
  const cands = [
    process.env.PLANAI_COMPONENTS,
    ...externalRoots().map((r) => path.join(r, 'components')),       // shared, beside the launcher
    app.isPackaged ? path.join(process.resourcesPath, 'components') : null, // embedded fallback
    path.join(__dirname, '..', '..', 'dist', 'components'),          // dev
  ].filter(Boolean);
  return cands.find((c) => fs.existsSync(path.join(c, 'manifest.json'))) || null;
}

function cacheRoot() {
  if (process.env.PLANAI_CACHE) return process.env.PLANAI_CACHE;
  const base = (() => { try { return app.getPath('cache'); } catch { return os.tmpdir(); } })();
  return path.join(base, 'plan-ai');
}

// Bundled static squashfs tools (squashfuse_ll + unsquashfs). They run on any
// linux (musl static), so they ship inside the linux/nixos artifacts.
function mountTools() {
  const dirs = [
    process.env.PLANAI_MOUNT_TOOLS,
    ...externalRoots().map((r) => path.join(r, 'tools', 'bin')),       // shared, beside the launcher
    app.isPackaged ? path.join(process.resourcesPath, 'tools', 'bin') : null, // embedded fallback
    path.join(__dirname, '..', '..', 'dist', 'tools', 'bin'),          // dev
  ].filter(Boolean);
  const find = (n) => { for (const d of dirs) { const p = path.join(d, n); if (fs.existsSync(p)) return p; } return null; };
  return { squashfuse: find('squashfuse_ll'), unsquashfs: find('unsquashfs') };
}

// the single runtime component shipped in this bundle (one per OS), by base name
function runtimeBase(comp) {
  const m = fs.readdirSync(comp).find((f) => /^runtime-.*\.(squashfs|tar\.gz|dmg)$/.test(f));
  return m ? m.replace(/\.(squashfs|tar\.gz|dmg)$/, '') : null;
}

// pick the on-disk component file for a base name, preferring mountable formats
function componentFile(comp, base) {
  const order = process.platform === 'darwin'
    ? ['.dmg', '.squashfs', '.tar.gz']
    : ['.squashfs', '.dmg', '.tar.gz'];
  for (const ext of order) {
    const p = path.join(comp, base + ext);
    if (fs.existsSync(p)) return { file: p, ext };
  }
  return null;
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
// rocm is only selected when its component is bundled AND /dev/kfd exists AND the
// ROCm runtime libs are installed — otherwise that build crashes on launch.
function detectOllama(comp) {
  const list = fs.readdirSync(comp);
  const has = (key) => list.some((f) => new RegExp(`^ollama-${key}\\.(squashfs|tar\\.gz|dmg)$`).test(f));
  const base = (key) => (has(key) ? `ollama-${key}` : null);
  const checks = [];
  const add = (key, usable, why) => checks.push({ flavour: key, bundled: has(key), usable, why });

  let chosenKey = null;
  if (process.platform === 'linux' && process.arch !== 'arm64') {
    const rocmBundled = has('linux-amd64-rocm');
    const kfd = fs.existsSync('/dev/kfd');
    const rocmLib = libPresent(['libamdhip64.so', 'libamdhip64.so.6', 'librocm-core.so']);
    const rocmUsable = rocmBundled && kfd && rocmLib;
    add('linux-amd64-rocm', rocmUsable,
      !rocmBundled ? 'not bundled in this build (CPU build)'
      : !kfd ? 'no AMD GPU (/dev/kfd absent)'
      : !rocmLib ? 'ROCm runtime not installed (libamdhip64 not found)'
      : 'AMD GPU + ROCm runtime detected');
    if (rocmUsable) chosenKey = 'linux-amd64-rocm';
    if (!chosenKey) {
      const cuda = libPresent(['libcuda.so', 'libcuda.so.1']);
      add('linux-amd64', has('linux-amd64'),
        cuda ? 'NVIDIA driver present — ollama uses CUDA, else CPU' : 'CPU (no usable GPU runtime detected)');
      if (has('linux-amd64')) chosenKey = 'linux-amd64';
    }
  } else if (process.platform === 'linux') {
    add('linux-arm64', has('linux-arm64'), 'arm64 CPU');
    if (has('linux-arm64')) chosenKey = 'linux-arm64';
  } else if (process.platform === 'darwin') {
    add('darwin', has('darwin'), 'macOS universal (Metal)');
    if (has('darwin')) chosenKey = 'darwin';
  } else if (process.platform === 'win32') {
    add('windows-amd64', has('windows-amd64'), 'Windows x64');
    if (has('windows-amd64')) chosenKey = 'windows-amd64';
  }

  const sel = checks.find((c) => c.flavour === chosenKey);
  return {
    base: chosenKey ? base(chosenKey) : null,
    flavour: chosenKey,
    reason: sel ? sel.why : 'no ollama flavour available for this machine',
    checks,
  };
}

let lastAccel = null;
function getAccel() { return lastAccel; }

// --- providing a component: mount in place, or extract ----------------------
const mounts = []; // { dest, type:'fuse'|'dmg' }

function isMountpoint(p) {
  try { return fs.statSync(p).dev !== fs.statSync(path.dirname(p)).dev; }
  catch { return false; }
}

function extractTar(archive, dest, marker) {
  if (fs.existsSync(marker)) return;
  fs.mkdirSync(dest, { recursive: true });
  tar.x({ file: archive, cwd: dest, sync: true }); // handles gzip transparently
  fs.writeFileSync(marker, path.basename(archive));
}

function extractSquashfs(img, dest, tools, marker, log) {
  if (fs.existsSync(marker)) return true;
  if (!tools.unsquashfs) { log('no bundled unsquashfs — cannot extract ' + path.basename(img)); return false; }
  fs.mkdirSync(dest, { recursive: true });
  execFileSync(tools.unsquashfs, ['-f', '-no-progress', '-d', dest, img], { stdio: 'pipe' });
  fs.writeFileSync(marker, path.basename(img));
  return true;
}

// libfuse calls a setuid `fusermount` helper to mount unprivileged. Like the
// AppImage runtime, find one anywhere (incl. NixOS's /run/wrappers/bin) and hand
// it to libfuse via FUSERMOUNT_PROG so squashfuse_ll mounts on more systems.
function findFusermount() {
  if (process.env.FUSERMOUNT_PROG) return process.env.FUSERMOUNT_PROG;
  const dirs = (process.env.PATH || '').split(':')
    .concat(['/run/wrappers/bin', '/usr/bin', '/bin', '/usr/local/bin']);
  for (const name of ['fusermount3', 'fusermount']) {
    for (const d of dirs) { const p = path.join(d, name); if (d && fs.existsSync(p)) return p; }
  }
  return null;
}

function tryMountSquashfs(img, dest, tools, log) {
  fs.mkdirSync(dest, { recursive: true });
  if (isMountpoint(dest)) { mounts.push({ dest, type: 'fuse' }); return true; }
  if (!tools.squashfuse) return false;
  const env = { ...process.env };
  const fm = findFusermount();
  if (fm) env.FUSERMOUNT_PROG = fm;
  try {
    // squashfuse_ll <image> <mountpoint>; needs /dev/fuse + a fusermount helper.
    execFileSync(tools.squashfuse, [img, dest], { stdio: 'pipe', env });
    if (isMountpoint(dest)) { mounts.push({ dest, type: 'fuse' }); return true; }
  } catch (e) { log('squashfuse mount failed (' + e.message.split('\n')[0] + ') — will extract'); }
  return false;
}

function tryMountDmg(img, dest, log) {
  fs.mkdirSync(dest, { recursive: true });
  if (isMountpoint(dest)) { mounts.push({ dest, type: 'dmg' }); return true; }
  try {
    execFileSync('hdiutil', ['attach', '-nobrowse', '-noverify', '-mountpoint', dest, img], { stdio: 'pipe' });
    mounts.push({ dest, type: 'dmg' });
    return true;
  } catch (e) { log('hdiutil attach failed (' + e.message.split('\n')[0] + ')'); return false; }
}

// Make component <base> available at <dest>. forceExtract bypasses mounting when
// the consumer needs a WRITABLE tree (ollama on NixOS, which must be patchelf'd).
function provide(comp, base, dest, root, log, opts = {}) {
  const found = componentFile(comp, base);
  if (!found) throw new Error('component not found: ' + base);
  const { file, ext } = found;
  const tools = mountTools();
  if (ext === '.squashfs') {
    if (!opts.forceExtract && tryMountSquashfs(file, dest, tools, log)) { log(`mounted ${base} (squashfs)`); return; }
    log(`extracting ${base} (squashfs)`);
    if (extractSquashfs(file, dest, tools, path.join(root, `.${base}.unsq.done`), log)) return;
    throw new Error('could not mount or extract ' + file + ' (no FUSE and no unsquashfs)');
  }
  if (ext === '.dmg') {
    if (!opts.forceExtract && tryMountDmg(file, dest, log)) { log(`mounted ${base} (dmg)`); return; }
    const gz = path.join(comp, base + '.tar.gz'); // extraction fallback shipped alongside
    if (fs.existsSync(gz)) { log(`extracting ${base} (tar.gz fallback)`); extractTar(gz, dest, path.join(root, `.${base}.done`)); return; }
    throw new Error('could not mount ' + file + ' (no .tar.gz fallback)');
  }
  // .tar.gz
  log(`extracting ${base} (tar.gz)`);
  extractTar(file, dest, path.join(root, `.${base}.done`));
}

function unmountAll() {
  for (const m of mounts.splice(0)) {
    try {
      if (m.type === 'fuse') {
        const fm = findFusermount();
        if (fm) execFileSync(fm, ['-u', m.dest], { stdio: 'pipe' });
        else try { execFileSync('fusermount', ['-u', m.dest], { stdio: 'pipe' }); }
             catch { execFileSync('fusermount3', ['-u', m.dest], { stdio: 'pipe' }); }
      } else if (m.type === 'dmg') {
        execFileSync('hdiutil', ['detach', m.dest], { stdio: 'pipe' });
      }
    } catch { /* best effort on quit */ }
  }
}

// Provide the needed components into <cache>/dist and point PLANAI_RESOURCES there.
// No-op (returns false) when there is no components/ dir (plain dev tree).
function prepare(log = () => {}) {
  // dev: if a runtime is already staged in dist/ (scripts/dev.sh / run-nixos.sh),
  // use it directly — don't provide components (faster, and what the dev tests
  // expect). Only relevant unpackaged; packaged artifacts always provide.
  if (!app.isPackaged) {
    const dev = path.join(__dirname, '..', '..', 'dist');
    if (fs.existsSync(path.join(dev, 'runtime', 'venv')) || fs.existsSync(path.join(dev, 'runtime', 'python'))) {
      return false;
    }
  }
  const comp = componentsDir();
  if (!comp || !runtimeBase(comp)) return false;

  const root = path.join(cacheRoot(), 'root');
  const dist = path.join(root, 'dist');
  fs.mkdirSync(dist, { recursive: true });

  const rt = runtimeBase(comp);
  const accel = detectOllama(comp);
  lastAccel = accel;
  if (!rt) throw new Error('no runtime component in ' + comp);
  if (!accel.base) throw new Error('no ollama flavour for ' + process.platform + '/' + process.arch);
  log(`ollama flavour: ${accel.flavour} — ${accel.reason}`);

  // NixOS: mounting via FUSE needs a fusermount helper, and ollama must be
  // writable to repoint its interpreter at the nix loader (the bare nix-ld stub
  // can't run a generic ELF). So on NixOS extract everything (via the bundled
  // static unsquashfs — no FUSE). Real Linux mounts in place (squashfuse_ll).
  const forceExtract = !!process.env.PLANAI_NIX_LD;
  provide(comp, rt, path.join(dist, 'runtime'), root, log, { forceExtract });
  if (componentFile(comp, 'ow-assets')) provide(comp, 'ow-assets', path.join(dist, 'ow-assets'), root, log, { forceExtract });

  const ollamaDest = path.join(dist, 'ollama');
  provide(comp, accel.base, ollamaDest, root, log, { forceExtract });

  if (process.env.PLANAI_NIX_LD) {
    const marker = path.join(root, `.${accel.base}.patched`);
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

module.exports = { prepare, componentsDir, getAccel, unmountAll };
