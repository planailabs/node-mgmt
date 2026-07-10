# node-mgmt

A portable, **offline** AI stack on a USB stick. A native **rust launcher**
supervises [Ollama](https://github.com/ollama/ollama) (local LLM server) and
[Open-WebUI](https://github.com/open-webui/open-webui) (chat UI), serves a
[Dioxus](https://dioxuslabs.com) dashboard + control API on `127.0.0.1`, and
points a **thin Electron** webview at it — no system Python, Node, or internet
required at runtime.

Everything is **built on NixOS** (the dev box), including the Windows and macOS
artifacts. NixOS itself is not a separate target: the linux build ships a
`buildFHSEnv` helper closure and re-execs the generic binaries inside a
bubblewrap sandbox, so the same artifact runs on ordinary Linux **and** NixOS.
The dashboard reuses [`plan-ai-design`](https://git.plan.ai/plan-ai/design)
(a git submodule, consumed as real Dioxus components).

```
┌────────── per-OS artifact (standalone launcher: linux/win exe, mac dmg) ─────┐
│  rust launcher  (control plane, static-musl on linux; cross for win/mac)     │
│   ├─ mounts components/<os>/  (squashfs / dmg — mounted, not extracted)       │
│   ├─ supervises:  ollama serve   +   uvicorn open_webui.main:app             │
│   ├─ serves on 127.0.0.1:8088 →  Dioxus SPA (dashboard)  +  /api/* control   │
│   └─ launches thin Electron → loadURL(PLANAI_UI_URL)                          │
└───────────────────────────────────────────────────────────────────────────────┘
   <drive>/models → OLLAMA_MODELS        <drive>/data → Open-WebUI DATA_DIR
```

---

## Contents

- [Architecture](#architecture) · [Prerequisites](#prerequisites) · [Single source of truth](#single-source-of-truth)
- [Flow A — Dev (run on NixOS)](#flow-a--dev-run-on-nixos)
- [Flow B — Prod (build shippable artifacts)](#flow-b--prod-build-shippable-artifacts)
- [Flow C — Ready-to-burn USB image](#flow-c--ready-to-burn-usb-image)
- [Features & components](#features--components) · [Models](#models) · [llmfit](#llmfit-gpu-detection--model-browser)
- [Codesigning](#codesigning) · [Testing](#testing) · [Targets](#targets) · [Layout](#layout) · [NixOS notes](#nixos-notes)

For the deep reference (constraints, gotchas, why each choice was made) read
[`AGENTS.md`](AGENTS.md); for the build framework read
[`third_party/loader/AGENTS.md`](third_party/loader/AGENTS.md).

---

## Architecture

- **Rust launcher = control plane** (`launcher/`). On every platform it mounts
  the `components/` it needs, starts the supervisor (`mac-mgmt-services`, from
  the `third_party/mac-mgmt` submodule) which spawns + restarts ollama +
  uvicorn, serves the embedded SPA + control API on `127.0.0.1` (`PLANAI_UI_PORT`,
  default 8088), then runs Electron. `launcher/src/`: `config.rs` (child env),
  `paths.rs` (resource resolution), `control.rs` (supervisor + health),
  `serve.rs` (axum: SPA + `/api/*`), `proxy.rs` (llmfit proxy).
- **Dioxus SPA** (`launcher/spa-src/`): a web/wasm app reusing `plan-ai-design`,
  built by `dx` to static assets the launcher rust-embeds (`launcher/spa/`) and
  serves. Dashboard (service cards, live-log SSE), Models (llmfit GPU-aware
  browser + ollama download), and an embedded Open-WebUI iframe. Same-origin
  `/api/*`. Built via `make spa` / `nix build .#spa`.
- **Thin Electron** (`app/`): just `main/index.js` — a window that
  `loadURL(PLANAI_UI_URL)`. No node deps, no renderer, no supervisor.
- **Components are nix-built and mounted, not extracted.** Every component
  (python runtime, ollama flavours, open-webui assets, the Electron `app-<os>`,
  the mac `.app`/`.dmg`) is packed by a nix derivation into `components/<os>/`
  as a squashfs (linux/win) or dmg (mac). The launcher squashfuse-mounts / dmg-
  attaches them at startup — no unpack, no store closure shipped.
- **usbd** (`usbd/`): the node-management daemon, built on the extracted
  `mac-mgmt-agent` crate; its reduced config types live in `crates/usb-config`
  (shared with `mock-server/`). `crates/control-api` holds the `/api/*` types.
- **Two python runtimes:** the **portable** runtime (python-build-standalone +
  pip wheels, relocatable, generic interp — for shipped artifacts) and the
  **dev** runtime (a nixpkgs-python venv — `make dev`, runs on NixOS). Neither
  ships a `/nix/store` reference in the artifact.

## Prerequisites

[Nix](https://nixos.org) with flakes. Everything else is in the devshell:

```sh
git clone --recurse-submodules <repo> && cd node-mgmt
nix develop      # node22, python312, uv, electron, rust+wasm32, dx, dioxus
                 # toolchain, rcodesign, wine, mtools, dosfstools, … + submodules
```

The devshell is the **only** supported environment for every command below
(`make` refuses to run outside it, except `clean`/`help`).

## Single source of truth

[`loader.toml`](loader.toml) is the one declarative manifest the shared build
tool (`xtask`, in the `third_party/loader` submodule) derives **everything**
from: targets, features, components, the FHS package set, the on-drive update
manifest, the drive layout, and the Makefile target list. The
[`Makefile`](Makefile) is *generated* from it (`make makefile`) but committed so
it can bootstrap `xtask`.

Version/asset pins live in [`usb.lock`](usb.lock) (→ `vendor.lock.json` FOD
hashes). Current pins:

```jsonc
{
  "ollama":    { "repo": "ollama/ollama",         "version": "v0.30.7" },
  "openwebui": { "repo": "open-webui/open-webui",  "version": "v0.9.6" },
  "llmfit":    { "repo": "AlexsJones/llmfit",       "version": "v0.9.31" },
  "hermes":    { "repo": "NousResearch/hermes-agent", "version": "v2026.5.16" },
  "llamacpp":  { "repo": "ggml-org/llama.cpp",      "version": "b9601" },
  "python": "3.12.13", "pbs_release": "20260602",
  "targets": ["linux-x64","linux-arm64","win-x64","mac-arm64","nixos-x64","nixos-arm64"],
  "models":  ["smollm2:1.7b"]
}
```

To bump a dependency: edit `usb.lock`, run `make update` (regens vendor/uv/npm
locks), commit. `make update-deps` bumps everything to latest first.

---

## Flow A — Dev (run on NixOS)

Fastest inner loop: a nix-native runtime + the real rust launcher + the SPA,
under the nixpkgs Electron. Idempotent — re-runs skip completed steps.

```sh
nix develop
make dev          # minimal NixOS build + run (dev runtime + launcher + SPA)
```

**Piece-wise dev** against the real bundled launcher — build one piece locally
and swap it in (needs a bundle once: `make components bundle TARGET=linux-x64`):

```sh
make dev-spa        # locally built SPA        (--with-spa)
make dev-electron   # local app/ tree          (--with-electron; cd app && npm i)
make dev-usbd       # locally built usbd        (--with-usbd)
make dev-usbd-spa   # both usbd + SPA
```

**UI-only** iteration (no launcher):

```sh
make ui             # SPA against the mock backend (dx serve + mock API on :9999)
make spa            # build the SPA into launcher/spa/ (nix build .#spa)
```

## Flow B — Prod (build shippable artifacts)

Each artifact = the rust launcher + the thin Electron app + the chosen ollama
flavour + a relocatable python runtime, with everything packed as mounted
`components/<os>/`.

```sh
nix develop
make download                       # materialise FOD downloads into vendor/ (cached)
make all                            # download → wheel → app → runtimes → components → bundles → image

# or per target (TARGET = linux-x64 | linux-arm64 | win-x64 | mac-arm64):
make wheel                          # open-webui frontend + wheel + offline assets
make runtime  TARGET=linux-x64      # relocatable python runtime (nix)
make components                     # pack modular component archives (runtimes + ollama flavours + assets)
make bundle   TARGET=linux-x64      # single-file artifact for the target
```

Per-target artifacts (electron-builder `dir` for linux/win, `@electron/packager`
+ `rcodesign` for mac): a standalone **rust launcher** per OS (`node-mgmt.linux-
x64.exe` / `node-mgmt.exe` / `node-mgmt.dmg`) beside the shared `components/<os>/`
pool that holds the Electron app as an `app-<target>` component. CPU-only torch
keeps every artifact **under 4 GiB** (fits FAT32, no split needed).

## Flow C — Ready-to-burn USB image

One **FAT32** image with every platform's launcher + `components/<os>/` + models
at the root (each binary finds the shared `models/` + `data/`). The image is
itself a nix `runCommand` (`mkfs.vfat`+`mcopy`, offline, no root):

```sh
make bundle TARGET=linux-x64
make bundle TARGET=win-x64
make bundle TARGET=mac-arm64
make image                          # → dist/plan-ai-node-mgmt.img  (FAT32)

sudo dd if=dist/plan-ai-node-mgmt.img of=/dev/sdX bs=4M status=progress conv=fsync
```

`make image` auto-runs `make models`. There's a hard 4 GiB per-file guard, so
no exFAT and no artifact splitting are needed.

---

## Features & components

Optional stacks are declared as **features** in `loader.toml` and packed as
independent components; the on-drive `platforms.json` selects which ship, and
the update manifest heals same-version installs.

| feature | default | what it adds |
|---|---|---|
| `openwebui` | **on** | Open-WebUI chat UI + its python runtime + assets |
| `hermes` | off | Hermes agent dashboard (`:9119`) + shared hermes-webui (`:8787`) |
| `llamacpp` | off | llama.cpp server (`:8090`) over ollama ggufs / `models/gguf` |

`ollama` itself is **core** (the dashboard talks to it directly), not gated by
`openwebui`. Select at build time via `PLANAI_PLATFORMS` / the `usb.lock`
`targets`; per-feature via the component `feature` tags.

## Models

```sh
# add to usb.lock: "models": ["smollm2:1.7b", "qwen2.5:7b"]
make models                          # (a.k.a. make seed) pulls into ./models (shared on the USB)
```

## llmfit (GPU detection + model browser)

[`llmfit`](https://github.com/AlexsJones/llmfit) (MIT, rust) is bundled beside
the launcher. The launcher runs `llmfit system --json` to detect GPU/VRAM/backend
(shown in the dashboard "Acceleration" panel) and `llmfit serve` to back the
model browser, **proxied** same-origin through `/api/llmfit/*`. Upstream's
prebuilt binaries are bundled (linux static-musl runs on NixOS too). Pins:
`usb.lock` `.llmfit.version` → `make update-locks` → `vendor.lock.json` FODs.

## Codesigning

- **macOS** — the `.app` is signed with `rcodesign` (ad-hoc by default). Real
  identity: `MAC_P12=cert.p12 MAC_P12_PASS=… make bundle TARGET=mac-arm64`.
- **Windows** — the `zip` target is unsigned; Authenticode-sign the inner exe
  with `osslsigncode` (`WIN_PFX`/`WIN_PFX_PASS`).

## Testing

```sh
make test          # lint + runtime import + live ollama/open-webui health (NixOS)
make test-nixos    # launch the built nixos bundle under xvfb + screenshot
make test-usb      # FAT32 loop-image launch test (models/data on FAT32; needs sudo)
make test-vm       # run the bundled launcher in an Ubuntu 26.04 incus VM
make test-mac      # run the mac launcher on a remote mac (MAC_TARGET=<ssh host>)
make test-win      # run the win launcher on a remote windows box (WIN_TARGET=<ssh host>)
make test-all      # build/health + nixos + FAT32 + ubuntu VM (+mac/win if targets set)
make test-clean    # wipe outputs and rebuild from scratch (TARGET=linux-x64)
```

## Targets

| target | builds on NixOS | notes |
|---|---|---|
| linux-x64 (launcher + components) | ✅ | the reference path; runs on generic Linux **and** NixOS |
| linux-arm64 (launcher + components) | ✅ | same path, aarch64 |
| win-x64 (launcher .exe + components) | ✅ | `nsis`/`portable` installers would need a Windows runner |
| mac-arm64 (launcher .dmg + components) | ✅ | `rcodesign` ad-hoc in nix; notarization needs macOS |

`nixos-x64` / `nixos-arm64` are **aliases** of the linux targets (same artifact,
FHS helper). **mac-x64 (Intel) is dropped** — torch/brotlicffi ship arm64-only
macOS wheels, so the x86_64-darwin cross-install is unsatisfiable.

## Layout

```
loader.toml          single declarative manifest (targets/features/components/FHS/layout/make)
usb.lock             version pins  ·  vendor.lock.json  FOD hashes
flake.nix            devshell + vendor/ollama FODs + .#spa + launcher-<target> + .#xtask (from the submodule)
Makefile             GENERATED from loader.toml [make] (committed to bootstrap xtask)
launcher/            rust control plane: src/ + spa-src/ (Dioxus SPA) + spa/ (embedded build)
app/                 thin Electron shell (main/index.js)
usbd/                node-management daemon (on mac-mgmt-agent)
crates/              control-api (/api types) · usb-config (shared config)
mock-server/         mock /api backend for `make ui`
hermes/ runtime/     per-target wheel locks (hermes agent · open-webui python runtime)
scripts/             impure project steps (download, build-openwebui, make-runtime, seed, tests)
nix/                 builds.nix (component packers) · runtime.nix · vendor.nix
third_party/loader   the build framework submodule (git@git.plan.ai:plan-ai/loader-builder)
third_party/mac-mgmt supervisor + agent crates
third_party/plan-ai-design  design system (Dioxus components; submodule)
```

## NixOS notes

NixOS can't run generic FHS binaries (bare nix-ld stub), which shapes several
choices:

- The linux artifact ships a `buildFHSEnv` helper closure as
  `nixos-fhs.squashfs`; the static-musl launcher squashfuse-mounts it and — in
  an outer bubblewrap namespace — provides it as `/nix/store` (overlay union
  where unprivileged overlayfs works, else a plain bind that replaces it), then
  re-execs inside the sandbox so the generic electron/ollama run. No
  `nix-store --import`, so no trusted-user requirement.
- electron-builder's helpers (`mksquashfs`, `appimagetool`, `makensis`) are
  `patchelf`'d to the nix loader at pack time; `USE_SYSTEM_7ZA=true`.
- Nix libs are passed to **child processes only** via
  `PLANAI_CHILD_LD_LIBRARY_PATH` — a global `LD_LIBRARY_PATH` makes nixpkgs
  electron crash with **SIGILL**.
- The prod python runtime is cross-installed (never executed at build), so one
  NixOS host builds all targets.
