# plan-ai-usb-minimal

A portable, **offline** AI stack on a USB stick. One binary launches
[Ollama](https://github.com/ollama/ollama) (local LLM server) and
[Open-WebUI](https://github.com/open-webui/open-webui) (chat UI) inside an
Electron desktop app that supervises both services and embeds the chat UI — no
system Python, Node, or internet required at runtime.

Everything is **built on NixOS** (the dev box), including the Windows and macOS
artifacts. The dashboard is styled with
[`plan-ai-design`](https://git.plan.ai/plan-ai/design) (consumed as a Tailwind
layer; a git submodule).

```
┌──────────── single artifact (AppImage / win .zip / mac .app.zip) ───────────┐
│  Electron shell                                                             │
│   ├─ dashboard (service status · start/stop · logs)   ← plan-ai-design CSS  │
│   ├─ embedded <webview> → http://127.0.0.1:8080  (Open-WebUI)              │
│   └─ supervises:  ollama serve   +   uvicorn open_webui.main:app           │
└─────────────────────────────────────────────────────────────────────────────┘
   <usb>/models → OLLAMA_MODELS        <usb>/data → Open-WebUI DATA_DIR
```

---

## Contents

- [Concepts](#concepts) · [Prerequisites](#prerequisites) · [usb.lock](#usblock)
- [Flow A — Dev (run on NixOS)](#flow-a--dev-run-on-nixos)
- [Flow B — Prod (build shippable artifacts)](#flow-b--prod-build-shippable-artifacts)
- [Flow C — Ready-to-burn USB image](#flow-c--ready-to-burn-usb-image)
- [Models](#models) · [Codesigning](#codesigning) · [Testing](#testing) · [CI](#ci)
- [Platform support](#platform-support) · [Scripts](#scripts-reference) · [NixOS notes](#nixos-notes)

---

## Concepts

There are **two python runtime strategies**, by design:

| | how it's built | runs on | used by |
|---|---|---|---|
| **dev runtime** | a venv from the **nixpkgs** Python | NixOS (native) | `make dev` / `run-nixos.sh` |
| **prod runtime** | **python-build-standalone** + `uv pip install --python-platform` (cross) | the target OS (generic Linux / Windows / macOS) | `bundle.sh` artifacts |

The prod runtime is cross-installed (it never runs the target interpreter), so a
single NixOS host can build all three OS artifacts. The dev runtime exists
because those generic binaries can't run on NixOS (the bare nix-ld stub) — for
local iteration you use nix-native tooling instead.

## Prerequisites

[Nix](https://nixos.org) with flakes. Everything else is in the devshell:

```sh
git clone --recurse-submodules <repo> && cd plan-ai-usb-minimal
nix develop      # node22, python312, uv, electron, rcodesign, wine, nsis,
                 # osslsigncode, mtools, dosfstools, qemu, … + submodule init
```

The devshell is the **only** supported environment for every command below.

## usb.lock

Single source of truth every script reads ([`usb.lock`](usb.lock)):

```jsonc
{
  "ollama":    { "repo": "ollama/ollama",        "version": "v0.30.6" },
  "openwebui": { "repo": "open-webui/open-webui", "version": "v0.9.6" },
  "python":      "3.12.13",        // pinned CPython (nix + python-build-standalone)
  "pbs_release": "20260602",       // python-build-standalone release tag
  "targets":  ["linux-x64", "win-x64", "mac-arm64", "mac-x64"],
  "models":   ["smollm2:1.7b"]     // pre-seeded onto the USB
}
```

---

## Flow A — Dev (run on NixOS)

Fastest inner loop. Builds a nix-native runtime and launches the real stack via
the nixpkgs Electron. Idempotent — re-runs skip completed steps.

```sh
nix develop
make dev          # = scripts/dev.sh
```

What it does: init the design submodule → download the `linux-amd64` ollama
flavour → build the Open-WebUI wheel + prefetch offline assets → create a
nix-native venv with Open-WebUI → build Tailwind CSS → stage into `dist/` →
launch the dashboard.

UI-only iteration (after `make dev` once):

```sh
cd app && npm start          # electron with hot-ish reload of renderer
npm run css:watch            # rebuild tailwind on design/markup changes
```

The dev launcher is `scripts/run-nixos.sh` (staging + nixpkgs electron). Models
and data default to `app/.run/` in dev; override with `PLANAI_PORTABLE_ROOT`.

## Flow B — Prod (build shippable artifacts)

Each artifact = the Electron app + the chosen ollama flavour + a relocatable
python runtime for the target, packaged as a single file.

```sh
nix develop
make download                       # ollama (all flavours) + open-webui source
make wheel                          # open-webui frontend + wheel + offline assets

# per target (TARGET = linux-x64 | win-x64 | mac-arm64 | mac-x64):
make runtime TARGET=linux-x64       # python-build-standalone + cross-installed deps
make app                            # npm ci + tailwind
make bundle  TARGET=linux-x64       # → dist/bundle/plan-ai-<ver>-linux-x64.AppImage
```

Or the whole linux pipeline at once: `make all` (defaults to host target).

Outputs in `dist/bundle/`:

| TARGET | artifact | packaged with |
|---|---|---|
| `linux-x64` | `plan-ai-<ver>-linux-x64.AppImage` | electron-builder |
| `win-x64`   | `plan-ai-<ver>-win-x64.zip`        | electron-builder (zip) |
| `mac-arm64` / `mac-x64` | `plan-ai-<ver>-mac-<arch>.zip` (signed `.app`) | @electron/packager + rcodesign |

Minimal builds: fetch just one ollama flavour, e.g.
`./scripts/download-ollama.sh ollama-linux-amd64.tar.zst`. Pick a GPU flavour for
a target with `./scripts/bundle.sh linux-x64 --flavour ollama-linux-amd64-rocm.tar.zst`.

## Flow C — Ready-to-burn USB image

Collect the per-platform artifacts + models into one FAT32 image with all three
bundles at the root (so each binary finds the shared `models/` + `data/`):

```sh
make bundle TARGET=linux-x64
make bundle TARGET=win-x64
make bundle TARGET=mac-arm64
make image                           # → dist/plan-ai-usb.img  (FAT32, no root needed)

sudo dd if=dist/plan-ai-usb.img of=/dev/sdX bs=4M status=progress conv=fsync
```

Filesystem choice (`--fs auto|fat32|exfat`, default `auto`):

- **exFAT** (auto-picked when a file >4 GiB, e.g. the 5.3G AppImage) — loop mount, needs sudo.
- **FAT32** — universal; the linux AppImage exceeds FAT32's 4 GiB/file limit, so
  `--fs fat32` **auto-splits** it into `<name>.AppImage.partNN` (<4 GiB each) plus a
  `<name>.run.sh` launcher that reassembles + verifies (sha256) + runs it:

  ```sh
  make image FS=fat32        # or: ./scripts/make-usb-image.sh --fs fat32
  # on the stick: ./plan-ai-<ver>-linux-x86_64.run.sh   (joins parts -> cache, launches)
  ```

  Split a standalone AppImage yourself with `./scripts/split-appimage.sh`.

---

## Models

```sh
# add to usb.lock: "models": ["smollm2:1.7b", "qwen2.5:7b"]
make seed                            # pulls them into ./models (shared on the USB)
```

## Codesigning

- **macOS** — `bundle.sh mac-*` signs the `.app` with `rcodesign`. Ad-hoc by
  default; for a real identity:
  `MAC_P12=cert.p12 MAC_P12_PASS=… ./scripts/bundle.sh mac-arm64`.
- **Windows** — the `zip` target is unsigned. To Authenticode-sign the inner
  `plan.ai.exe`, sign with `osslsigncode` (provide `WIN_PFX`/`WIN_PFX_PASS`); the
  hook in `bundle.sh` covers the exe-target case.

## Testing

```sh
make test          # lint + open_webui import + live ollama/open-webui /health (NixOS)
make test-usb      # FAT32 loop image: models/data on FAT32, launch, screenshot (needs sudo)
make test-vm       # run the AppImage in an Ubuntu 26.04 incus VM (needs KVM)
make test-clean    # wipe build outputs and rebuild + verify (TARGET=linux-x64)
make test-clean -- --full   # also wipe the vendor/ download cache
```

All are runnable on the NixOS dev box (sudo for loop mount; KVM + incus for the
VM). They produce screenshots under `/tmp/*.png`.

## CI

[`.gitlab-ci.yml`](.gitlab-ci.yml) runs every job in the nix devshell:

- `lint` → `build:linux` / `build:win` / `build:mac` (matrix) → `usb-image`
- `test:usb-fat32` and `test:ubuntu-vm` need a privileged/KVM runner (tagged).

Windows/macOS builds run from the Linux runner (cross). For nsis/dmg installers
or notarization, add native Windows/macOS runners.

## Platform support

| target | builds on NixOS | verified runnable | notes |
|---|---|---|---|
| linux-x64 (AppImage) | ✅ | ✅ NixOS dev + Ubuntu 26.04 VM | the reference path |
| win-x64 (zip) | ✅ | ⚠️ not run-tested here | extract + run `plan.ai.exe`; nsis/portable need a real wine prefix or Windows runner |
| mac-arm64 / mac-x64 (.app.zip) | ✅ | ⚠️ not run-tested here | rcodesign ad-hoc; `.dmg` + notarization need macOS |

## Scripts reference

| script | purpose |
|---|---|
| `lib.sh` | shared: usb.lock accessors, GitHub API, sha256, pbs/triple maps |
| `download-ollama.sh [names…]` | fetch ollama flavours (filter by substring), verify, manifest |
| `download-openwebui.sh` | fetch + verify open-webui source |
| `build-openwebui.sh` | npm build + `uv build` wheel + prefetch embedding/nltk assets |
| `make-runtime.sh [target]` | python-build-standalone + cross-installed Open-WebUI |
| `bundle.sh [target] [--flavour]` | package the single-file artifact (+ codesign) |
| `make-usb-image.sh [out] [--fs auto\|fat32\|exfat]` | USB image with all bundles + models (fat32 auto-splits the AppImage) |
| `split-appimage.sh [file] [--chunk-mb N]` | split a >4 GiB artifact into FAT32-sized parts + a reassembly launcher |
| `seed-models.sh [dir]` | pre-pull usb.lock models |
| `dev.sh` / `run-nixos.sh` | NixOS dev build + launcher |
| `test-*.sh` | build/health, FAT32, ubuntu-vm, clean-build tests |

## NixOS notes

NixOS can't run generic FHS binaries (bare nix-ld stub), which shapes several choices:

- electron-builder's prebuilt helpers (`mksquashfs`, `appimagetool`, `makensis`)
  are `patchelf`'d to the nix loader at pack time (the embedded AppImage runtime
  is left generic); `USE_SYSTEM_7ZA` uses the nix 7za.
- The prod python runtime is cross-installed (never executed at build).
- The dev stack passes nix libs to child processes via
  `PLANAI_CHILD_LD_LIBRARY_PATH` only — never to electron (global `LD_LIBRARY_PATH`
  SIGILLs nixpkgs electron).
- Windows `portable`/`nsis` targets execute the exe under wine (needs a full
  prefix); the `zip` target avoids this.
