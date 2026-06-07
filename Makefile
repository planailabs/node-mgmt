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

.PHONY: all dev download download-curl vendor-lock update ollama openwebui wheel \
        runtime runtimes app spa components bundle bundles image seed test \
        test-usb test-vm test-nixos test-clean test-all clean help

all: download wheel app runtimes components bundles image ## build EVERY target (mac/win/linux/nixos) + image

runtimes: ## build the python runtime for every target
	@for t in $(ALLTGTS); do $(MAKE) --no-print-directory runtime TARGET=$$t; done

components: ## pack modular component archives (runtimes + all ollama flavours + assets)
	./scripts/build-components.sh

bundles: ## package every target from the components
	@for t in $(ALLTGTS); do $(MAKE) --no-print-directory bundle TARGET=$$t; done

dev: ## minimal NixOS build + run (development mode)
	./scripts/dev.sh

download: ## materialise vendored downloads from Nix FODs (cached) into vendor/
	./scripts/fetch-vendor.sh

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
	./scripts/build-openwebui.sh

runtime: ## relocatable python runtime for TARGET
	./scripts/make-runtime.sh $(TARGET)

app: ## install the thin Electron shell deps
	cd app && npm ci

spa: ## build the Dioxus SPA dashboard into launcher/spa/ (nix build .#spa)
	./scripts/build-spa.sh

bundle: ## package single-file artifact for TARGET
	./scripts/bundle.sh $(TARGET)

image: ## ready-to-burn FAT32 USB image (all artifacts < 4 GiB; reads everywhere)
	./scripts/make-usb-image.sh


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
	rm -rf dist app/node_modules app/renderer/tailwind.css app/.stage app/.stage.lock

help: ## list targets
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | sort | \
	  awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
