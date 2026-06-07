'use strict';
// All component loading is done by the native rust launcher (plan-ai / plan-ai.exe
// / plan.ai.app): it mounts the app + runtime + ollama + ow-assets components, sets
// PLANAI_RESOURCES, and detects the GPU via llmfit (PLANAI_GPU_JSON) before starting
// Electron. This module does NO mounting/hdiutil/squashfs — it only reads what the
// launcher exported. Dev (unpackaged): scripts/dev.sh stages a runtime in dist/ that
// paths.js resolves directly (no launcher, no mounting).

// Acceleration summary for the dashboard, from the launcher's env.
function getAccel() {
  let gpu = null;
  try { gpu = JSON.parse(process.env.PLANAI_GPU_JSON).system || null; } catch { /* none */ }
  return {
    flavour: process.env.PLANAI_OLLAMA_FLAVOUR || null,
    reason: process.env.PLANAI_OLLAMA_REASON || null,
    llmfitUrl: process.env.PLANAI_LLMFIT_URL || null,
    gpu,
  };
}

// The launcher already prepared the runtime; nothing to mount here.
function prepare(log = () => {}) {
  const ready = !!process.env.PLANAI_RESOURCES;
  log(ready ? 'runtime prepared by native launcher' : 'dev: using staged dist/ runtime');
  return ready;
}

// The launcher owns the component mounts and releases them on exit.
function unmountAll() {}

module.exports = { prepare, getAccel, unmountAll };
