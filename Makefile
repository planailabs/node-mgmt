# plan-ai-usb-minimal — pipeline entrypoints.
# Run inside `nix develop` (Linux build leg). TARGET defaults to the host.
TARGET ?=
# TARGET prunes the xtask/ninja graph: with TARGET set, components/bundles are
# generated for that platform only (PLANAI_PLATFORMS is xtask's selection knob;
# unset → usb.lock .targets = everything, the update-server/CI flow).
ifneq ($(TARGET),)
export PLANAI_PLATFORMS ?= $(TARGET)
endif

# Subset the whole build (runtimes, components, bundles, image) to specific
# platforms instead of usb.lock's full .targets. Space/comma separated, e.g.
#   make image PLATFORMS=linux-x64
# xtask reads PLANAI_PLATFORMS when (re)generating the ninja graph.
# Defaults to TARGET, so `make components bundle TARGET=linux-x64` builds that
# platform's graph only (set PLATFORMS explicitly to widen/override).
PLATFORMS ?= $(TARGET)
export PLANAI_PLATFORMS = $(PLATFORMS)

# --- nix develop guard ------------------------------------------------------
# Every target except clean/help needs the devshell toolchain (node, python,
# uv, electron, rcodesign, mtools, …). The devshell sets PLANAI_DEVSHELL=1.
# Fail fast with a helpful message instead of cryptic "command not found".
GUARDED := $(filter-out clean help,$(or $(MAKECMDGOALS),all))
ifneq ($(GUARDED),)
ifndef PLANAI_DEVSHELL
$(error not in the devshell — run 'nix develop' first, or 'nix develop --command make $(MAKECMDGOALS)')
endif
endif

# Distributable targets built by `make all` (+ the nixos target).
# mac-x64 (Intel) is omitted: modern Python wheels (torch, brotlicffi, …) ship
# macOS arm64-only, so an x86_64-darwin cross-install is unsatisfiable. Apple
# Silicon (mac-arm64) is the supported macOS target.
# NixOS is not a separate target: the linux-x64 bundle ships the FHS helper
# closure and the static-musl launcher FHS-reexecs on NixOS, so one linux build
# runs everywhere (incl. NixOS). `make dev` covers local NixOS iteration.
TARGETS := linux-x64 win-x64 mac-arm64
ALLTGTS := $(TARGETS)

# Artifact builds go through ninja (via the xtask orchestrator) so EVERY sub-step
# is dependency-tracked — no stale "just there" outputs feeding a later step. The
# Makefile stays the entrypoint; it just delegates to the ninja graph. xtask is
# built offline by nix so this works in CI too.
# NB: '#' starts a Make comment — escape it (\#) so the flake attr survives.
XTASK := nix run .\#xtask --

.PHONY: all dev ui download download-curl vendor-lock update update-deps ollama openwebui wheel \
        runtime runtimes app spa components bundle bundles image update-tarball tarball-upload ninja seed models test \
        test-usb test-vm test-nixos test-clean test-mac test-win test-all clean help \
        dev-spa dev-electron dev-usbd

all: ## build EVERY target (mac/win/linux/nixos) + image (via ninja)
	$(XTASK) build all

ninja: ## (re)generate build.ninja from the artifact graph
	$(XTASK) gen-ninja

runtimes: ## build the python runtime for every target
	$(XTASK) build runtimes

components: ## pack modular component archives (runtimes + all ollama flavours + assets)
	$(XTASK) build components

bundles: ## package every target from the components
	$(XTASK) build bundles

update-tarball: ## update-server tarball (manifest.json + files/) for the update URL
	$(XTASK) build update-tarball

tarball-upload: update-tarball ## upload the update tarball to a web-agency webspace (set WEB_AGENCY_TOKEN + WEB_AGENCY_URL [+ WEB_AGENCY_WEBSPACE_ID])
	$(XTASK) upload

dev: ## minimal NixOS build + run (development mode)
	./scripts/dev.sh

# dev-* run the BUILT bundle launcher (make bundle TARGET=linux-x64) with one
# locally-built piece overriding its component — everything else (components,
# update flow) stays the launcher's own. See the --start-with-* flags.
dev-spa: ## launch the bundle with a locally built SPA (scripts/build-spa.sh)
	./scripts/build-spa.sh
	dist/bundle/plan-ai.linux-x64.exe --start-with-spa launcher/spa

dev-electron: ## launch the bundle with the local app/ tree (needs: cd app && npm i)
	dist/bundle/plan-ai.linux-x64.exe --start-with-electron app

dev-usbd: ## launch the bundle with a locally built usbd (cargo build --release)
	cd usbd && cargo build --release
	dist/bundle/plan-ai.linux-x64.exe --start-with-usbd usbd/target/release/usbd

ui: ## preview the SPA against the mock backend (dx serve + mock API on :9999)
	@echo "==> tailwind + mock API (:9999) + dx serve (hot reload)"
	( cd launcher/spa-src && tailwindcss -i ../../third_party/plan-ai-design/assets/input.css \
	    -o assets/tailwind.css --config tailwind.config.js ) ; \
	  cargo run --manifest-path mock-server/Cargo.toml & \
	  trap 'kill %1 2>/dev/null' EXIT INT TERM ; \
	  ( cd launcher/spa-src && dx serve | cat )

download: ## materialise vendored downloads from Nix FODs (cached) into vendor/
	$(XTASK) build download

vendor-lock: ## regenerate vendor.lock.json (run when usb.lock bumps)
	./scripts/gen-vendor-lock.sh

update: ## regenerate ALL locks after bumping usb.lock (vendor + uv + npm)
	./scripts/update-locks.sh

update-deps: ## bump ollama/open-webui/llmfit/pbs/python to latest + regen locks
	./scripts/update-deps.sh

# legacy curl-based fetch (no Nix); FODs (make download) are preferred
download-curl: ollama openwebui
ollama:
	./scripts/download-ollama.sh
openwebui:
	./scripts/download-openwebui.sh

wheel: ## build open-webui frontend+wheel + prefetch offline assets
	$(XTASK) build wheel

runtime: ## relocatable python runtime for TARGET
	$(XTASK) build runtime-$(TARGET)

app: ## install the thin Electron shell deps
	$(XTASK) build app

spa: ## build the Dioxus SPA dashboard into launcher/spa/ (nix build .#spa)
	$(XTASK) build spa

bundle: ## package single-file artifact for TARGET
	$(XTASK) build bundle-$(TARGET)

image: ## ready-to-burn FAT32 USB image (all artifacts < 4 GiB; reads everywhere)
	$(XTASK) build image


seed models: ## pre-pull usb.lock models into ./models (cached; also auto-run by `make image`)
	$(XTASK) build models

test: ## lint + runtime import + live ollama/open-webui health
	./scripts/test-build.sh

test-usb: ## FAT32 loop-image launch test (models/data on FAT32)
	./scripts/test-usb-image.sh

test-vm: ## run the AppImage in an Ubuntu 26.04 incus VM
	./scripts/test-ubuntu-vm.sh

test-nixos: ## launch the built nixos bundle under xvfb + screenshot
	./scripts/test-nixos.sh

test-clean: ## wipe build outputs and rebuild from scratch (TARGET=linux-x64)
	./scripts/test-clean-build.sh $(TARGET)

test-mac: ## run the mac launcher on a remote mac (set MAC_TARGET=<ssh host>)
	./scripts/test-mac.sh

test-win: ## run the win launcher on a remote windows box (set WIN_TARGET=<ssh host>)
	./scripts/test-win.sh

test-all: ## every test (build/health, nixos, FAT32, ubuntu VM; +mac/win if MAC_TARGET/WIN_TARGET set)
	./scripts/test-build.sh
	./scripts/test-nixos.sh
	./scripts/test-usb-image.sh
	./scripts/test-ubuntu-vm.sh
	@if [ -n "$(MAC_TARGET)" ]; then ./scripts/test-mac.sh; else echo "== skip test-mac (MAC_TARGET unset) =="; fi
	@if [ -n "$(WIN_TARGET)" ]; then ./scripts/test-win.sh; else echo "== skip test-win (WIN_TARGET unset) =="; fi

clean: ## remove all build outputs (ninja graph + dist + every crate's target/)
	@if [ -f build.ninja ] && command -v ninja >/dev/null 2>&1; then \
	  echo "ninja -t clean"; ninja -f build.ninja -t clean >/dev/null 2>&1 || true; fi
	rm -rf dist build.ninja .ninja_log .ninja_lock \
	       app/node_modules app/.stage app/.stage.lock \
	       launcher/target launcher/spa-src/target launcher/spa-src/.cargo \
	       spinner/target xtask/target mock-server/target crates/*/target

help: ## list targets
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | sort | \
	  awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
