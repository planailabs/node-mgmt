# plan-ai-usb-minimal

A portable, **offline** AI stack you carry on a USB stick. One binary launches
[Ollama](https://github.com/ollama/ollama) (local LLM server) and
[Open-WebUI](https://github.com/open-webui/open-webui) (chat UI), wrapped in an
Electron desktop app that supervises both services and embeds the chat UI
directly — no system Python, Node, or internet required at runtime.

The dashboard is styled with [`plan-ai-design`](https://git.plan.ai/plan-ai/design)
(consumed as a Tailwind layer; included as a git submodule).

```
┌──────────────── single binary (AppImage / .exe / .dmg) ─────────────────┐
│  Electron shell                                                          │
│   ├─ dashboard (service status · start/stop · logs)                      │
│   ├─ embedded <webview> → http://127.0.0.1:8080  (Open-WebUI)            │
│   └─ supervises:  ollama serve   +   uvicorn open_webui.main:app         │
└──────────────────────────────────────────────────────────────────────────┘
   <usb>/models  → OLLAMA_MODELS      <usb>/data → Open-WebUI DATA_DIR
```

## Pinned versions — `usb.lock`

All versions are pinned in [`usb.lock`](usb.lock) (JSON), the single source of
truth every script reads:

```json
{ "ollama": { "version": "v0.30.6" },
  "openwebui": { "version": "v0.9.6" },
  "python": "3.12.8" }
```

## Prerequisites

- [Nix](https://nixos.org) with flakes. Everything else comes from the devshell:

```sh
git clone --recurse-submodules <this-repo>
cd plan-ai-usb-minimal
nix develop          # node 22, python 3.12, uv, electron, packaging tools
```

## Build (Linux leg)

```sh
make all             # download → wheel → runtime → bundle
# or step by step:
make download        # all 17 ollama flavours (sha256-verified) + open-webui source
make wheel           # open-webui frontend build + wheel + offline assets
make runtime         # relocatable python (python-build-standalone + uv venv)
make app             # npm ci + tailwind build
make bundle          # → dist/bundle/plan-ai-<ver>-linux-x64.AppImage
```

Run the dashboard in development without packaging:

```sh
cd app && npm start
```

## NixOS dev mode (`make dev`)

The generic AppImage targets ordinary Linux and won't run on NixOS (the bare
nix-ld stub can't start FHS binaries). For developing/testing **on NixOS**, use
the nix-native dev mode — it runs the full stack (Ollama + Open-WebUI + the
dashboard) using nixpkgs Electron, a venv built from the nixpkgs Python, and the
real pinned ollama (patchelf'd to the nix loader):

```sh
nix develop
make dev            # minimal build (linux-amd64 only) + launch; idempotent
```

This fetches just the `linux-amd64` ollama flavour, builds Open-WebUI once, makes
a nix-native venv, stages everything into `dist/`, and launches the dashboard.
Re-runs skip completed steps for a fast loop. Verified: both services reach
"ready" and the embedded Open-WebUI renders, fully offline.

## Per-OS artifacts

`make bundle` packages for the **host** OS only — the python runtime and
Open-WebUI's native deps (chromadb/onnxruntime/…) must be built on the matching
OS. All three legs run in CI:

| Target      | Runner          | Artifact            |
|-------------|-----------------|---------------------|
| linux-x64   | ubuntu-latest   | `.AppImage`         |
| win-x64     | windows-latest  | `.exe` (portable+nsis) |
| mac-arm64   | macos-14        | `.dmg` / `.zip`     |
| mac-x64     | macos-13        | `.dmg` / `.zip`     |

See [`.github/workflows/build.yml`](.github/workflows/build.yml). The download
step fetches **all** ollama flavours; the bundler selects the right one per
target (override with `./scripts/bundle.sh <target> --flavour <asset>` for
GPU builds: rocm/mlx/jetpack).

## Offline kiosk behaviour

All network access happens at **build** time. At runtime the app sets
`WEBUI_AUTH=False`, `HF_HUB_OFFLINE=1`, `TRANSFORMERS_OFFLINE=1`, points
`HF_HOME`/`NLTK_DATA` at the pre-bundled assets, and writes `DATA_DIR`/
`OLLAMA_MODELS` next to the binary on the USB. The embedding model
(`all-MiniLM-L6-v2`) and nltk data are bundled so RAG works without internet.

## Seeding models

Models are **not** bundled (kept "minimal"). List them in `usb.lock` and pull
them onto the USB ahead of time:

```jsonc
// usb.lock
"models": ["llama3.2:3b", "qwen2.5:7b"]
```
```sh
make seed            # → ./models  (point the packaged app's <usb>/models at it)
```

## Layout

| Path | Purpose |
|------|---------|
| `usb.lock` | pinned versions (source of truth) |
| `flake.nix` | nix devshell (Linux build leg) |
| `scripts/` | download / build / runtime / bundle / seed |
| `app/` | Electron app (main supervisor + design-styled renderer) |
| `third_party/plan-ai-design/` | design system submodule (Tailwind layer only) |
| `vendor/`, `dist/` | download cache / build output (gitignored) |

## NixOS note

electron-builder ships generic prebuilt helpers (`mksquashfs`, etc.) that the
bare nix-ld stub can't run. The devshell sets `USE_SYSTEM_7ZA`/`NIX_LD` and
`bundle.sh` `patchelf`s the build-time helpers (never the embedded AppImage
runtime, which stays generic for real Linux targets).
