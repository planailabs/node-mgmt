#!/usr/bin/env bash
# Download ollama release flavours for the version pinned in usb.lock.
# Verifies each asset against the sha256 digest the GitHub API reports, and
# writes a manifest.json the bundler consumes to pick the per-OS binary.
#
# By default downloads ALL flavours. Pass one or more name substrings to limit
# the set (e.g. `download-ollama.sh linux-amd64` for a minimal single-target
# build), or set ONLY_ASSETS to a space-separated list.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

FILTERS=("$@")
[ ${#FILTERS[@]} -eq 0 ] && [ -n "${ONLY_ASSETS:-}" ] && read -ra FILTERS <<<"$ONLY_ASSETS"
matches_filter() {
  [ ${#FILTERS[@]} -eq 0 ] && return 0
  local n="$1" f
  for f in "${FILTERS[@]}"; do [[ "$n" == *"$f"* ]] && return 0; done
  return 1
}

REPO="$(ollama_repo)"
TAG="$(ollama_version)"
OUT="$VENDOR_DIR/ollama/$TAG"
mkdir -p "$OUT"

log "ollama $REPO@$TAG -> $OUT"
REL="$(release_json "$REPO" "$TAG")"

# Build a tab-separated table: name <tab> url <tab> sha256(bare) <tab> size
# The API exposes asset.digest as "sha256:<hex>" (may be null for old releases).
mapfile -t ROWS < <(printf '%s' "$REL" | jq -r '
  .assets[]
  | [ .name,
      .browser_download_url,
      (.digest // "" | sub("^sha256:";"")),
      (.size // 0 | tostring)
    ] | @tsv')

[ "${#ROWS[@]}" -gt 0 ] || die "no assets found for $REPO@$TAG"

# manifest accumulator (name -> {sha256,size,url})
MANIFEST="$OUT/manifest.json"
tmp_manifest="$(mktemp)"
echo '{}' > "$tmp_manifest"

count=0
for row in "${ROWS[@]}"; do
  IFS=$'\t' read -r name url sha size <<<"$row"
  matches_filter "$name" || continue
  download_verified "$url" "$OUT/$name" "${sha:--}"
  # If the API gave no digest, compute one so the manifest is always complete.
  [ -z "$sha" ] && sha="$(sha256_of "$OUT/$name")"
  jq --arg n "$name" --arg s "$sha" --arg z "$size" --arg u "$url" \
     '.[$n] = {sha256:$s, size:($z|tonumber), url:$u}' \
     "$tmp_manifest" > "$tmp_manifest.next" && mv "$tmp_manifest.next" "$tmp_manifest"
  count=$((count+1))
done

jq --arg repo "$REPO" --arg tag "$TAG" \
   '{repo:$repo, tag:$tag, assets:.}' "$tmp_manifest" > "$MANIFEST"
rm -f "$tmp_manifest"

log "ollama: $count assets downloaded + verified -> $MANIFEST"
