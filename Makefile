# plan-ai-usb-minimal — pipeline entrypoints.
# Run inside `nix develop` (Linux build leg). TARGET defaults to the host.
TARGET ?=

.PHONY: all dev download ollama openwebui wheel runtime app bundle image split \
        seed test test-usb test-vm test-clean clean

all: download wheel runtime bundle ## full pipeline -> dist/bundle

dev: ## minimal NixOS build + run (development mode)
	./scripts/dev.sh

download: ollama openwebui ## fetch ollama flavours + open-webui source

ollama:
	./scripts/download-ollama.sh

openwebui:
	./scripts/download-openwebui.sh

wheel: ## build open-webui frontend+wheel + prefetch offline assets
	./scripts/build-openwebui.sh

runtime: ## relocatable python runtime for TARGET
	./scripts/make-runtime.sh $(TARGET)

app: ## install app deps + build tailwind css
	cd app && npm ci && npm run css

bundle: ## package single-file artifact for TARGET
	./scripts/bundle.sh $(TARGET)

image: ## ready-to-burn USB image (FS=auto|fat32|exfat; fat32 auto-splits the AppImage)
	./scripts/make-usb-image.sh $(if $(FS),--fs $(FS))

split: ## split the linux AppImage into <4GiB parts + a reassembly launcher
	./scripts/split-appimage.sh

seed: ## pre-pull models from usb.lock into ./models
	./scripts/seed-models.sh

test: ## lint + runtime import + live ollama/open-webui health
	./scripts/test-build.sh

test-usb: ## FAT32 loop-image launch test (models/data on FAT32)
	./scripts/test-usb-image.sh

test-vm: ## run the AppImage in an Ubuntu 26.04 incus VM
	./scripts/test-ubuntu-vm.sh

test-clean: ## wipe build outputs and rebuild from scratch (TARGET=linux-x64)
	./scripts/test-clean-build.sh $(TARGET)

clean:
	rm -rf dist app/node_modules app/renderer/tailwind.css app/.stage app/.stage.lock
