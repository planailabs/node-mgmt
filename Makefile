# plan-ai-usb-minimal — pipeline entrypoints.
# Run inside `nix develop` (Linux build leg). TARGET defaults to the host.
TARGET ?=

.PHONY: all download ollama openwebui wheel runtime app bundle seed clean

all: download wheel runtime bundle ## full pipeline -> dist/bundle

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

seed: ## pre-pull models from usb.lock into ./models
	./scripts/seed-models.sh

clean:
	rm -rf dist app/node_modules app/renderer/tailwind.css app/.stage
