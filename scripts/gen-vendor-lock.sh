#!/usr/bin/env bash
# Generate vendor.lock.json — the URL + sha256 of every download (ollama
# flavours, python-build-standalone, open-webui source), so flake.nix can fetch
# them as fixed-output derivations (cached + content-addressed in /nix/store,
# binaries byte-identical because FODs run no fixup). Re-run when usb.lock bumps.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need jq; need curl
OUT="$REPO_ROOT/vendor.lock.json"
OLLAMA_REPO="$(ollama_repo)"; OLLAMA_TAG="$(ollama_version)"
OW_REPO="$(ow_repo)"; OW_TAG="$(ow_version)"
PYVER="$(py_version)"; PBS="$(pbs_release)"

# --- ollama: every flavour, from the releases API digests -------------------
log "ollama $OLLAMA_REPO@$OLLAMA_TAG"
# only the flavours the loader/bundles use (skip mlx/jetpack/dmg/etc — saves ~3GB)
OLLAMA_WANT='ollama-linux-amd64.tar.zst ollama-linux-arm64.tar.zst ollama-linux-amd64-rocm.tar.zst ollama-darwin.tgz ollama-windows-amd64.zip'
OLLAMA_ASSETS="$(release_json "$OLLAMA_REPO" "$OLLAMA_TAG" | jq -c --arg want "$OLLAMA_WANT" '
  ($want | split(" ")) as $w
  | [ .assets[]
      | select(.name as $n | $w | index($n))
      | { name: .name, url: .browser_download_url, sha256: (.digest // "" | sub("^sha256:";"")) }
      | select(.sha256 != "") ]')"

# --- python-build-standalone: our target triples ---------------------------
log "python-build-standalone $PYVER+$PBS"
PBS_REL="$(gh_curl "https://api.github.com/repos/astral-sh/python-build-standalone/releases/tags/$PBS")"
pbs_entry() {  # <target> <triple>
  local triple="$2" name url sha
  name="cpython-${PYVER}+${PBS}-${triple}-install_only.tar.gz"
  url="$(printf '%s' "$PBS_REL" | jq -r --arg n "$name" '.assets[] | select(.name==$n) | .browser_download_url')"
  sha="$(printf '%s' "$PBS_REL" | jq -r --arg n "$name" '.assets[] | select(.name==$n) | (.digest // "" | sub("^sha256:";""))')"
  [ -n "$url" ] && [ -n "$sha" ] || { warn "pbs $triple: no asset/digest"; return 0; }
  jq -nc --arg t "$1" --arg tr "$triple" --arg u "$url" --arg s "$sha" \
    '{target:$t, triple:$tr, url:$u, sha256:$s}'
}
PBS_ENTRIES="$(printf '%s\n' \
  "$(pbs_entry linux-x64 x86_64-unknown-linux-gnu)" \
  "$(pbs_entry mac-arm64 aarch64-apple-darwin)" \
  "$(pbs_entry win-x64 x86_64-pc-windows-msvc)" | jq -sc 'map(select(. != null and . != {}))')"

# --- open-webui source archive ---------------------------------------------
OW_URL="https://github.com/$OW_REPO/archive/refs/tags/$OW_TAG.tar.gz"
log "open-webui source $OW_TAG (hashing)"
OW_SHA="$(curl -fsSL "$OW_URL" | sha256sum | awk '{print $1}')"

# --- llmfit: upstream prebuilt binaries (sha256 from sidecar .sha256 files) --
# Cross-building llmfit hits toolchain walls (win synchronization.lib, mac libobjc),
# so we bundle its official prebuilt binaries; flake.nix fetches these as FODs.
LLMFIT_REPO="$(llmfit_repo)"; LLMFIT_TAG="$(llmfit_version)"
log "llmfit $LLMFIT_REPO@$LLMFIT_TAG (prebuilt binaries)"
llmfit_entry() {  # <target> <ext>
  local target="$1" ext="$2" url sha
  url="https://github.com/$LLMFIT_REPO/releases/download/${LLMFIT_TAG}/llmfit-${LLMFIT_TAG}-${target}.${ext}"
  sha="$(curl -fsSL "${url}.sha256" 2>/dev/null | awk '{print $1}')"
  [ -n "$sha" ] || { warn "llmfit $target: no .sha256 sidecar"; return 0; }
  jq -nc --arg t "$target" --arg e "$ext" --arg u "$url" --arg s "$sha" \
    '{target:$t, ext:$e, url:$u, sha256:$s}'
}
LLMFIT_ASSETS="$(printf '%s\n' \
  "$(llmfit_entry x86_64-unknown-linux-musl tar.gz)" \
  "$(llmfit_entry x86_64-pc-windows-msvc zip)" \
  "$(llmfit_entry aarch64-apple-darwin tar.gz)" | jq -sc 'map(select(. != null and . != {}))')"

jq -n \
  --arg otag "$OLLAMA_TAG" --argjson oassets "$OLLAMA_ASSETS" \
  --arg pyver "$PYVER" --arg pbs "$PBS" --argjson pbsent "$PBS_ENTRIES" \
  --arg owtag "$OW_TAG" --arg owurl "$OW_URL" --arg owsha "$OW_SHA" \
  --arg lftag "$LLMFIT_TAG" --argjson lfassets "$LLMFIT_ASSETS" '
{
  ollama:    { tag: $otag, assets: $oassets },
  pbs:       { python: $pyver, release: $pbs, files: $pbsent },
  openwebui: { tag: $owtag, url: $owurl, sha256: $owsha },
  llmfit:    { tag: $lftag, assets: $lfassets }
}' > "$OUT"

log "wrote $OUT — ollama:$(jq '.ollama.assets|length' "$OUT") pbs:$(jq '.pbs.files|length' "$OUT") ow:1 llmfit:$(jq '.llmfit.assets|length' "$OUT")"
