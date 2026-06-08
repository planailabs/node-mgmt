# plan-ai-usb-minimal — pipeline entrypoints.
# Run inside `nix develop` (Linux build leg). TARGET defaults to the host.
TARGET ?=

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
XTASK := nix run .#xtask --

.PHONY: all dev download download-curl vendor-lock update ollama openwebui wheel \
        runtime runtimes app spa components bundle bundles image update-tarball ninja seed test \
        test-usb test-vm test-nixos test-clean test-all clean help

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

dev: ## minimal NixOS build + run (development mode)
	./scripts/dev.sh

download: ## materialise vendored downloads from Nix FODs (cached) into vendor/
	$(XTASK) build download

vendor-lock: ## regenerate vendor.lock.json (run when usb.lock bumps)
	./scripts/gen-vendor-lock.sh

update: ## regenerate ALL locks after bumping usb.lock (vendor + uv + npm)
	./scripts/update-locks.sh

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


seed: ## pre-pull models from usb.lock into ./models
	./scripts/seed-models.sh

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

test-all: ## run every test (build/health, nixos bundle, FAT32 image, ubuntu VM)
	./scripts/test-build.sh
	./scripts/test-nixos.sh
	./scripts/test-usb-image.sh
	./scripts/test-ubuntu-vm.sh

clean:
	rm -rf dist app/node_modules app/.stage app/.stage.lock \
	       launcher/target launcher/spa-src/target

help: ## list targets
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | sort | \
	  awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
