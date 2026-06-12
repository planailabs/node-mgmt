# AGENTS.md

Guidance for AI agents (and humans) working on **plan-ai-usb-minimal** — a
portable, offline AI stack (Ollama + Open-WebUI behind a Dioxus dashboard) that is
**built on NixOS** and shipped for Linux, Windows, and macOS (NixOS runs the
regular linux build via the bundled FHS helper).

The control plane lives in a native **rust launcher** (`launcher/`): it mounts the
components, supervises ollama + open-webui (via `mac-mgmt-services`), and serves a
**Dioxus web SPA** (`launcher/spa-src/`, reusing `plan-ai-design`) + a control API
over `127.0.0.1`. **Electron is a thin webview** that loads that URL. The old
node main process (loader/supervisor/config/paths + the hand-written renderer) is
gone — see git history if you need it.

Read this before changing the build/packaging. It encodes constraints and
gotchas that are expensive to re-discover.

**Workflow: first test, then fix. Always add tests for new features.** Reproduce a
bug with a failing test before changing code; ship every new feature with tests
(unit tests in the owning crate, e.g. `crates/manifest`; end-to-end via the
`plan-ai self-update` subcommand against a local server; UI via `make ui` + the
mock-server).

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

NixOS is not a separate target: the linux artifact ships a buildFHSEnv helper closure
as a squashfs (`nixos-fhs.squashfs`), and the static-musl launcher squashfuse-mounts it
and — in an outer bubblewrap namespace — provides it as `/nix/store` (overlay union
where unprivileged overlayfs works, else a plain bind that replaces it), then re-execs
inside the FHS sandbox so the generic electron/ollama run. No `nix-store --import`, so
no trusted-user requirement. `make dev` covers local NixOS iteration.

---

## Standards

`standards/` holds the written conventions that span the codebase — each file is the
**source of truth** for one area (see `standards/README.md` for the index).

- **Read them first.** Before changing code in an area a standard covers (the `/api/*`
  control plane → `control-api.md`; any branch over a closed set like build targets /
  platforms / state enums → `exhaustive-matching.md`), read that standard and make the
  change conform. They exist because these classes of bug already bit us.
- **The convention lands here first.** If you intend to deviate from or extend a
  standard, update the standard in the same change — don't let code and standard
  drift. New cross-cutting convention ⇒ add a `standards/<area>.md`, register it in
  `standards/README.md`, and (if it's a rule agents must follow) link it here.

---

## Architecture

- **Rust launcher = control plane** (`launcher/`, static-musl on linux, cross-built
  for win/mac via cargo-zigbuild). On every platform it: mounts/extracts the
  `components/` it needs, sets `PLANAI_RESOURCES`, starts the **supervisor**
  (`mac-mgmt-services`, from the third_party/mac-mgmt submodule) which spawns + restarts ollama + uvicorn,
  serves the embedded SPA + control API on `127.0.0.1` (`PLANAI_UI_PORT`, default
  8088), exports `PLANAI_UI_URL`, then runs Electron. `launcher/src/`:
  `config.rs` (child env), `paths.rs` (resource resolution), `control.rs`
  (supervisor driver + health), `serve.rs` (axum: SPA + `/api/*`), `proxy.rs`
  (llmfit proxy). Resources resolve via `PLANAI_RESOURCES` (portable artifacts)
  or the dev layout staged in `dist/` (`scripts/dev.sh`).
- **Dioxus SPA** (`launcher/spa-src/`): a web/wasm app reusing `plan-ai-design`,
  built by `dx` to static assets that the launcher rust-embeds (`launcher/spa/`)
  and serves. Dashboard (service cards, facts, controls, live-log SSE), Models
  (llmfit GPU-aware browser + ollama download), and an embedded Open-WebUI
  iframe. Talks to the launcher's same-origin `/api/*`. Built via
  `make spa` / `nix build .#spa`; embedded automatically by the launcher build.
- **Thin Electron** (`app/`): just `main/index.js` — a window that
  `loadURL(PLANAI_UI_URL)`. No node runtime deps, no renderer, no supervisor.
- **Capability-based flavour selection.** The launcher checks real hardware/libs
  (rocm needs `/dev/kfd` **and** `libamdhip64` — `/dev/kfd` alone crashes the
  rocm build) and records *why* it chose a flavour; shown in the dashboard
  "Acceleration" panel (`PLANAI_OLLAMA_FLAVOUR`/`_REASON` → `/api/info`). Default
  is the CPU amd64 build; rocm is opt-in (`PLANAI_OLLAMA=rocm`) in a GPU build.
- **Two runtime kinds:** the **portable** runtime (python-build-standalone +
  pip wheels, relocatable, generic `/lib64` interp — for the shipped artifacts)
  and the **dev** runtime (a nixpkgs-python venv — `scripts/dev.sh`, runs on
  NixOS for local iteration). `launcher/src/paths.rs` resolves either via
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
  nullglob-free glob helper). The dev runtime (`scripts/dev.sh`) keeps a nix-native
  venv (store refs OK — it runs under the nixpkgs Electron on a nix host).

## Build & test (inside `nix develop`)

```
make download     # materialise FOD downloads (cached) into vendor/  (curl: download-curl)
make all          # download -> wheel -> app -> runtimes -> components -> bundles -> FAT32 image
make bundle TARGET=linux-x64|win-x64|mac-arm64
make spa          # build the Dioxus SPA into launcher/spa/ (nix build .#spa)
make image        # FAT32 image of all artifacts (no exFAT/split needed)
make dev          # build + launch on NixOS (dev runtime + rust launcher + SPA)
make test-all     # test-build, test-nixos, test-usb-image, test-ubuntu-vm
```

The launcher (and the SPA it embeds) build through the flake — `make bundle`
cross-builds `launcher-<target>` via `nix build`, which builds `.#spa` first and
embeds it. `nix develop` provides the SPA toolchain (rust+wasm32, `dx`,
`wasm-bindgen-cli` 0.2.121, binaryen, tailwind) for `make spa --dev`.

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
- **Components are nix-built + grouped per platform** (`components/<os>/`). Every
  component (runtime, ollama, ow-assets, the Electron `app-<os>`, the mac launcher
  `.app`+dmg) is packed by a nix derivation (`nix/builds.nix`: `mkSqfs`/`mkDmg`/
  dir; squashfs in a sandbox, dmg in a KVM VM, no sudo) consuming a content-
  addressed **store import** of the impure/electron-builder output. `bundle.sh`
  lays the bundle pool out as `components/<os>/` with a **declarative** per-OS
  `manifest.json` (the launcher uses it only as a pool marker — selection is by
  dir scan; `resolve_pool` in `launcher/src/main.rs` prefers `components/<os>/`,
  flat-pool fallback). The FAT32 **image** is itself a nix `runCommand`
  (`usb-image`): `make-usb-image.sh` stages a drive-root (launchers +
  `components/<os>/` + models + update.json + platforms.json) → store-import →
  `mkfs.vfat`+`mcopy` offline. `crates/manifest` `classify()` tags everything
  under `components/<target>/` to that target by the group segment. Real mac-cert
  (`MAC_P12`) + windows (`WIN_PFX`, network timestamp) signing stay imperative
  (secret/network inputs); the ad-hoc default is pure nix.
- **NixOS can't run generic FHS binaries** (bare nix-ld stub; `NIX_LD` ignored).
  - electron-builder's helpers (`mksquashfs`, `appimagetool`, `makensis`) are
    `patchelf`'d to the nix loader at pack time; `USE_SYSTEM_7ZA=true`.
  - Dev (`scripts/run-nixos.sh`) `patchelf`s the extracted ollama; the shipped
    linux build instead FHS-reexecs (the launcher squashfuse-mounts
    `nixos-fhs.squashfs` and bwrap-provides it as `/nix/store`, no import).
    Pass nix libs to **child processes only** via `PLANAI_CHILD_LD_LIBRARY_PATH`
    (`config.rs`) — a global `LD_LIBRARY_PATH` makes nixpkgs electron crash with
    **SIGILL**.
- **Windows:** use the electron-builder **`zip`** target. `portable`/`nsis`
  execute the built exe under wine, which fails on the minimal nix wine prefix
  (missing `ole32.dll`).
- **macOS:** built with `@electron/packager` (cross from Linux) + `rcodesign`
  (ad-hoc, or `MAC_P12`). `.dmg` needs macOS. **mac-x64 (Intel) is dropped** —
  torch/brotlicffi etc. ship macOS arm64-only wheels, so the x86_64-darwin
  cross-install is unsatisfiable. mac cross needs `MACOSX_DEPLOYMENT_TARGET=14.0`
  (onnxruntime ships only `macosx_14_0` wheels).
- **Open-WebUI 0.9.6 runtime env** (set in `launcher/src/config.rs`): `WEBUI_AUTH=False`,
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
  failure; `pack-component` skips runtimes without `open_webui`.
- **llmfit (GPU detection + model browser).** [`llmfit`](https://github.com/AlexsJones/llmfit)
  (MIT, rust) is bundled beside the launcher. The launcher runs `llmfit system
  --json` to detect the GPU/VRAM/backend (passed to Electron) and `llmfit serve`
  to back the dashboard's model browser. The launcher **proxies** llmfit through
  its own server (`serve.rs`/`proxy.rs` → `/api/llmfit/*`) so the SPA calls it
  same-origin (no CORS, port stays server-side). We **bundle upstream's prebuilt binaries**
  (not cross-built): cross-compiling it from NixOS hits walls its heavy deps need
  — **win** wants `synchronization.lib` (parking_lot/windows-sys), **mac** wants
  `libobjc`/the Apple SDK (objc2/sysinfo) — which zig doesn't bundle. linux uses
  their **static-musl** build (runs anywhere incl. NixOS, like our launcher).
  - **Pins live in `usb.lock` (`.llmfit.version`) → `vendor.lock.json`
    (`.llmfit.assets`, url+sha256 per target) → `flake.nix` FODs** (`llmfitBin`/
    `llmfit-{linux-x64,win-x64,mac-arm64}`). **Regenerate via `make update-locks`**
    (→ `gen-vendor-lock.sh`, which reads each release's `.sha256` sidecar). To bump:
    edit `usb.lock` `.llmfit.version`, run `make update-locks`, commit.

- **When touching `usb.lock` or `vendor.lock.json`, also update the scripts that
  work with them** — they are read in several places that don't share a parser:
  `scripts/lib.sh` (the `*_version`/`*_repo` jq helpers), `scripts/gen-vendor-lock.sh`
  (writes vendor.lock.json), `scripts/fetch-vendor.sh` (materialises vendor/ from
  the FODs), `scripts/update-locks.sh`, `nix/vendor.nix` (+ `flake.nix` consumers),
  and `xtask` (e.g. `ollama_tag()` reads usb.lock directly). Adding a key without
  threading it through these leaves the build half-wired.

## Pitfalls that bit us

- `make clean` mid-build wipes `dist/` + `node_modules` → app-builder "no such
  file" + npx refetch. Don't run it during a build.
- After `make all`, `dist/components/` (the flat staging pool) + `dist/bundle/
  components/<os>/` (the grouped bundle pool) exist, so the launcher would also
  extract; in dev it skips extraction when a dev runtime is staged in `dist/`
  (`PLANAI_DEV`/`PLANAI_RESOURCES`).
- The design system (`third_party/plan-ai-design`) is a **git submodule** and a
  Dioxus/Rust crate; the SPA consumes its **real components** (not CSS-only).
  Flakes exclude submodules, so `self.submodules = true` brings it into the
  `.#spa` build; keep the submodule rev and the SPA's `cargo-git-hashes.nix`
  (dioxus fork) in sync. `nix develop` auto-inits the submodule.
- The SPA pins `wasm-bindgen = "=0.2.121"` to match nixpkgs `wasm-bindgen-cli_0_2_121`;
  stock `dx` 0.7.9 prints a non-fatal "incompatible" notice for dioxus 0.8-alpha
  but builds fine. `wasm-opt` SIGABRTs in this toolchain (binaryen/LLVM feature
  mismatch) — non-fatal, dx ships the unoptimized wasm.

## Layout

`usb.lock` (pins) · `vendor.lock.json` (FOD hashes) · `flake.nix` (devshell +
`packages.vendor`/`ollamaComponents` FODs/repack + `.#spa` + `launcher-<target>`) ·
`scripts/` (download, build, runtime, components, bundle, image, build-spa, tests) ·
`launcher/` (rust control plane: `src/` + `spa-src/` Dioxus SPA + `spa/` embedded
build) · `app/` (thin Electron shell) · `third_party/plan-ai-design` (submodule) ·
`.gitlab-ci.yml` (build-all under nix).
