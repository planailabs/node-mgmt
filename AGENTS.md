# AGENTS.md

Guidance for AI agents (and humans) working on **plan-ai-usb-minimal** — a
portable, offline AI stack (Ollama + Open-WebUI in an Electron dashboard) that is
**built on NixOS** and shipped for Linux, Windows, macOS, and NixOS.

Read this before changing the build/packaging. It encodes constraints and
gotchas that are expensive to re-discover.

---

## The one rule that shapes everything

**Artifacts must run OUTSIDE the Nix store.** The AppImage / win zip / mac app /
FAT32 USB image run on ordinary machines with no Nix. Therefore:

- **Never ship a nixpkgs store closure.** `nixpkgs#open-webui` and `nixpkgs#ollama`
  exist (and match our versions!) but their wrappers point at `/nix/store/...`
  python + deps — unrunnable without the store. Do **not** use them for artifacts.
- **Nix is only a fetch + repack layer over already-built upstream assets.** Use it to
  download (fixed-output derivations) and to repack/normalise (raw derivations,
  no fixup) the upstream ollama binaries, python-build-standalone, pip wheels,
  and open-webui source. The **outputs are copied out of the store** and are
  byte-identical, relocatable archives.
- When building in Nix, prefer the **raw `derivation` builtin** (nix-pills ch.6),
  NOT `stdenv.mkDerivation` — stdenv's `fixupPhase` runs patchelf/strip/shebang
  rewriting that **corrupts the relocatable pbs interpreter and the generic
  cross-platform binaries**. Pre-fetch all external deps as FODs so the builder
  runs offline/pure.

The NixOS bundle is the one exception — it targets NixOS, so it may rely on the
nix store + nixpkgs electron.

---

## Architecture

- **Component model.** Each artifact ships compressed `components/` (the python
  runtime, the offline assets, and the ollama flavours for its OS) as Electron
  `extraResources`. The **in-app loader** (`app/main/loader.js`) runs in the
  main process on *every* platform: on first launch it extracts only what the
  machine needs (its one runtime + the ollama flavour matching the CPU
  arch/GPU) into a cache, sets `PLANAI_RESOURCES`, then the supervisor starts
  ollama + uvicorn. Lazy + idempotent.
- **Capability-based flavour selection.** The loader checks real hardware/libs
  (rocm needs `/dev/kfd` **and** `libamdhip64` — `/dev/kfd` alone crashes the
  rocm build) and records *why* each candidate was/wasn't chosen; shown in the
  dashboard "Acceleration" panel. Default is the CPU amd64 build; rocm is opt-in
  (`PLANAI_OLLAMA=rocm`) and only in a GPU build.
- **Two runtime kinds:** the **portable** runtime (python-build-standalone +
  pip wheels, relocatable, generic `/lib64` interp — for the shipped artifacts)
  and the **dev** runtime (a nixpkgs-python venv — `scripts/dev.sh`, runs on
  NixOS for local iteration). `app/main/paths.js` resolves either via
  `PLANAI_RESOURCES`.
- **Layer 3 — the runtime is a wheels-FOD, built by nix, run outside the store.**
  `make-runtime.sh` is **nix-only** (no pbs-download/uv-cross fallback). uv resolves
  (`runtime/uv.lock`); `scripts/gen-wheels-lock.sh` selects each target's compatible
  wheels + SRI hashes into `runtime/wheels-<target>.lock.json` (one run does all of
  linux-x64/win-x64/mac-arm64; mac matcher must include `universal2`). `nix/runtime.nix`
  fetches every wheel as an FOD (a wheelhouse) and a **vanilla** `derivation` (no
  stdenv/fixup) does `uv pip install --target --python-platform <triple> --no-index
  --offline open-webui` into the pbs tree. `python3` is on PATH only for uv's
  interpreter *discovery*; `--python-platform`/`--python-version` drive wheel
  selection and no wheel is executed, so all three targets cross-build on NixOS.
  Output is proven portable (generic interp, `$ORIGIN` rpath, **0 `/nix/store`
  refs**) and copied OUT of the store into `dist/runtime/<t>/python/`. Per-wheel
  FODs auto-dedup the ~213 shared pure-python wheels across platforms.
  Gotchas: Windows site-packages is `Lib/site-packages` (root `python.exe`), unix is
  `lib/pythonX.Y/site-packages` — an unexpanded glob silently makes a bogus `python*`
  dir. `compgen` is **unavailable** in nix-develop's non-interactive bash (use a
  nullglob-free glob helper). `nixos-x64` keeps a nix-native venv (store refs OK — it
  launches under nixpkgs Electron on a nix host).

## Build & test (inside `nix develop`)

```
make download     # materialise FOD downloads (cached) into vendor/  (curl: download-curl)
make all          # download -> wheel -> runtimes -> components -> bundles -> FAT32 image
make bundle TARGET=linux-x64|win-x64|mac-arm64|nixos-x64
make image        # FAT32 image of all artifacts (no exFAT/split needed)
make dev          # build + launch on NixOS (dev runtime)
make test-all     # test-build, test-nixos, test-usb-image, test-ubuntu-vm
```

`make` has a `nix develop` guard (PLANAI_DEVSHELL); only `clean`/`help` run outside.

## Key facts & gotchas

- **Size: CPU-only torch.** On Linux, pypi torch pulls ~4 GB of NVIDIA CUDA
  wheels the kiosk never uses. `make-runtime` installs torch from the PyTorch CPU
  index (`--extra-index-url .../cpu --index-strategy unsafe-best-match`) → no
  nvidia deps, no eager CUDA preload. Runtime 6.8 G → ~3 G, AppImage 5.3 G → ~3.3 G
  (fits FAT32, so **no exFAT and no AppImage splitting needed**). Do NOT just
  strip `nvidia_*` from a CUDA torch — it then fails on `libcublasLt` preload.
- **FAT32.** Every artifact stays < 4 GiB → plain FAT32 via mtools (no root).
  `make-usb-image` has a hard 4 GiB guard.
- **NixOS can't run generic FHS binaries** (bare nix-ld stub; `NIX_LD` ignored).
  - electron-builder's helpers (`mksquashfs`, `appimagetool`, `makensis`) are
    `patchelf`'d to the nix loader at pack time; `USE_SYSTEM_7ZA=true`.
  - The dev/nixos launcher `patchelf`s the extracted ollama; pass nix libs to
    **child processes only** via `PLANAI_CHILD_LD_LIBRARY_PATH` — a global
    `LD_LIBRARY_PATH` makes nixpkgs electron crash with **SIGILL**.
- **Windows:** use the electron-builder **`zip`** target. `portable`/`nsis`
  execute the built exe under wine, which fails on the minimal nix wine prefix
  (missing `ole32.dll`).
- **macOS:** built with `@electron/packager` (cross from Linux) + `rcodesign`
  (ad-hoc, or `MAC_P12`). `.dmg` needs macOS. **mac-x64 (Intel) is dropped** —
  torch/brotlicffi etc. ship macOS arm64-only wheels, so the x86_64-darwin
  cross-install is unsatisfiable. mac cross needs `MACOSX_DEPLOYMENT_TARGET=14.0`
  (onnxruntime ships only `macosx_14_0` wheels).
- **Open-WebUI 0.9.6 runtime env** (set in `app/main/config.js`): `WEBUI_AUTH=False`,
  persistent `WEBUI_SECRET_KEY` + `OAUTH_SESSION_TOKEN_ENCRYPTION_KEY` (required;
  stored under DATA_DIR), `FRONTEND_BUILD_DIR` pointed at the installed
  `open_webui/frontend` (env.py's default is wrong for an installed wheel),
  `HF_HUB_OFFLINE=1`/`TRANSFORMERS_OFFLINE=1` + both `HF_HOME` and
  `SENTENCE_TRANSFORMERS_HOME` at the prefetched assets, `NLTK_DATA`. Create
  `DATA_DIR` before launch (sqlite open).
- **electron pinned exact** (`41.7.1`, matches nixpkgs) + `electronVersion` in
  `electron-builder.yml`; newer electron-builder refuses a ranged version.
  `bundle.sh` runs `npm ci` if needed and uses `npx --no-install` so it can never
  fetch a different builder. Keep `app/package-lock.json` committed + in sync.
- **Never ship a partial runtime.** `make-runtime` removes `dist/runtime/<t>` on
  failure; `build-components` skips runtimes without `open_webui`.
- **llmfit (GPU detection + model browser).** [`llmfit`](https://github.com/AlexsJones/llmfit)
  (MIT, rust) is bundled beside the launcher. The launcher runs `llmfit system
  --json` to detect the GPU/VRAM/backend (passed to Electron) and `llmfit serve`
  to back the dashboard's model browser (`/api/v1/system`, `/api/v1/models/top`,
  `POST /api/v1/download` → ollama pull). We **bundle upstream's prebuilt binaries**
  (not cross-built): cross-compiling it from NixOS hits walls its heavy deps need
  — **win** wants `synchronization.lib` (parking_lot/windows-sys), **mac** wants
  `libobjc`/the Apple SDK (objc2/sysinfo) — which zig doesn't bundle. linux uses
  their **static-musl** build (runs anywhere incl. NixOS, like our launcher).
  - **Pins live in `usb.lock` (`.llmfit.version`) → `vendor.lock.json`
    (`.llmfit.assets`, url+sha256 per target) → `flake.nix` FODs** (`llmfitBin`/
    `llmfit-{linux-x64,win-x64,mac-arm64}`). **Regenerate via `make update-locks`**
    (→ `gen-vendor-lock.sh`, which reads each release's `.sha256` sidecar). To bump:
    edit `usb.lock` `.llmfit.version`, run `make update-locks`, commit.

## Pitfalls that bit us

- `make clean` mid-build wipes `dist/` + `node_modules` → app-builder "no such
  file" + npx refetch. Don't run it during a build.
- After `make all`, `dist/components/` exists, so the dev launcher would also
  extract; the loader skips extraction when a dev runtime is staged in `dist/`.
- The design system (`third_party/plan-ai-design`) is consumed as a **Tailwind
  layer only** (it's a Dioxus/Rust crate) — map its `--c-*` tokens in
  `app/tailwind.config.js`; no Rust toolchain.

## Layout

`usb.lock` (pins) · `vendor.lock.json` (FOD hashes) · `flake.nix` (devshell +
`packages.vendor`/`ollamaComponents` FODs/repack) · `scripts/` (download, build,
runtime, components, bundle, image, tests) · `app/` (Electron: main loader +
supervisor + design-styled renderer) · `.gitlab-ci.yml` (build-all under nix).
