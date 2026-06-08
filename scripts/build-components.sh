#!/usr/bin/env bash
# Build modular component archives consumed by the in-app loader (main/loader.js)
# on every platform. Each OS artifact ships the components/ for its OS; the app
# provides only what the machine needs (its runtime + the ollama flavour matching
# the CPU arch) on first launch — by MOUNTING in place where possible, else
# extracting.
#
# Per-OS component format (the loader prefers mounting over extraction):
#   linux / nixos : <name>.squashfs  -> mounted via bundled static squashfuse
#                                        (fallback: extracted via static unsquashfs)
#   windows       : <name>.tar.gz    -> extracted (pure-JS tar in the loader)
#   macOS         : <name>.tar.gz    -> extracted (HFS+ .dmg/hdiutil: a later pass)
# ow-assets is shared by every OS, so it is emitted in BOTH squashfs and tar.gz.
#
# Outputs dist/components/{<name>.squashfs|.tar.gz, manifest.json}.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need tar; need jq
OUT="$DIST_DIR/components"; rm -rf "$OUT"; mkdir -p "$OUT"
OLLAMA_TAG="$(ollama_version)"
OLLAMA_DIR="$VENDOR_DIR/ollama/$OLLAMA_TAG"

# parallel gzip (pigz) when available; output is plain gzip (the node loader's
# `tar` reads it). falls back to gzip.
if command -v pigz >/dev/null 2>&1; then GZIP_CMD="pigz -p $(nproc)"; else GZIP_CMD="gzip"; fi

# emit a directory as one or more component formats next to manifest.json.
emit_gz()   { tar -C "$1" -cf - . | $GZIP_CMD > "$OUT/$2.tar.gz"; log "+ $2.tar.gz ($(du -h "$OUT/$2.tar.gz" | cut -f1))"; }
emit_sqfs() { need mksquashfs; rm -f "$OUT/$2.squashfs"
  mksquashfs "$1" "$OUT/$2.squashfs" -comp zstd -processors "$(nproc)" -all-root -no-xattrs -noappend -quiet
  log "+ $2.squashfs ($(du -h "$OUT/$2.squashfs" | cut -f1)) [squashfs]"; }
# raw HFS+ image macOS mounts via hdiutil (no compression tool needed). Auto-size
# from the content (du) + headroom via a sparse truncate (no fixed dd count).
# Needs mkfs.hfsplus (hfsprogs) + loop-mount (sudo); returns non-zero to let the
# caller fall back to .tar.gz when unavailable (e.g. CI without sudo).
emit_dmg() {
  command -v mkfs.hfsplus >/dev/null 2>&1 || { warn "no mkfs.hfsplus — skip $2.dmg"; return 1; }
  local dir="$1" name="$2" img="$OUT/$2.dmg" mnt sz raw
  raw="$(mktemp -u).rawhfs"
  sz=$(du -sb "$dir" | cut -f1); sz=$(( sz * 11 / 10 + 64*1024*1024 ))   # +10% +64MB HFS+ overhead
  truncate -s "$sz" "$raw"
  mkfs.hfsplus -v PlanAI "$raw" >/dev/null 2>&1 || { warn "mkfs.hfsplus failed: $name"; rm -f "$raw"; return 1; }
  mnt="$(mktemp -d)"
  if ! sudo mount -o loop,umask=0000 "$raw" "$mnt" 2>/dev/null; then
    warn "loop-mount failed for $name.dmg (sudo?) — tar.gz fallback"; rmdir "$mnt"; rm -f "$raw"; return 1; fi
  sudo cp -a "$dir/." "$mnt/" && sudo umount "$mnt"; rmdir "$mnt" 2>/dev/null || true
  # Compress bare HFS+ -> proper UDIF dmg (Finder-mountable, ~3x smaller) via
  # libdmg-hfsplus. Falls back to the bare image if the tool is unavailable.
  rm -f "$img"
  local DMGTOOL; DMGTOOL="$(cd "$REPO_ROOT" && nix build .#libdmg-hfsplus --no-link --print-out-paths 2>/dev/null)/bin/dmg"
  if [ -x "$DMGTOOL" ] && "$DMGTOOL" dmg "$raw" "$img" >/dev/null 2>&1; then
    rm -f "$raw"; log "+ $name.dmg ($(du -h "$img" | cut -f1)) [UDIF compressed]"
  else
    warn "libdmg-hfsplus unavailable — bare HFS+ dmg for $name"; mv "$raw" "$img"
    log "+ $name.dmg ($(du -h "$img" | cut -f1)) [bare hfsplus]"
  fi
}
# windows: ship the component PRE-EXTRACTED as a plain directory. The win pbs
# tree has no symlinks/special perms, so it lives on FAT32 with no special flags
# and the loader uses it IN PLACE (no first-launch extraction, no mount tooling).
emit_dir() { rm -rf "$OUT/$2"; mkdir -p "$OUT/$2"; cp -a "$1/." "$OUT/$2/"
  log "+ $2/ (dir, $(du -sh "$OUT/$2" | cut -f1))"; }
# <srcdir> <name> <fmt...>   fmt in {gz,sqfs,dmg,dir}
emit() { local dir="$1" name="$2"; shift 2; local f
  for f in "$@"; do case "$f" in
    gz)   emit_gz "$dir" "$name" ;;
    sqfs) emit_sqfs "$dir" "$name" ;;
    dmg)  emit_dmg "$dir" "$name" || { [ -f "$OUT/$name.tar.gz" ] || emit_gz "$dir" "$name"; } ;;
    dir)  emit_dir "$dir" "$name" ;;
  esac; done; }
# which formats a component name ships in (drives the per-OS bundle staging).
#   linux/nixos = squashfs (mounted/extracted) ; macOS = dmg (hdiutil mount) ;
#   windows = pre-extracted dir (used in place). No .tar.gz is produced anymore.
fmts_for() { case "$1" in
  *linux-*|*nixos-*) echo sqfs ;;
  *windows-*|*win-*) echo dir ;;
  *darwin*|*mac-*)   echo dmg ;;
  ow-assets)         echo "sqfs dmg dir" ;; # shared: linux(sqfs) mac(dmg) win(dir)
  *)                 echo gz ;;
esac; }

# True if a glob matches at least one existing path. Portable substitute for
# `compgen -G`, which is unavailable in the non-interactive bash `nix develop`
# provides. Relies on an unmatched glob staying literal (no nullglob).
glob_exists() { local m; for m in $1; do [ -e "$m" ] && return 0; done; return 1; }

# shared offline assets (both formats — every OS bundle uses ow-assets)
[ -d "$VENDOR_DIR/ow-assets" ] && emit "$VENDOR_DIR/ow-assets" ow-assets $(fmts_for ow-assets)

# runtimes (one archive per built target)
RUNTIMES="[]"
for rt in "$DIST_DIR"/runtime/*/; do
  t="$(basename "$rt")"
  [ -d "$rt/venv" ] || [ -d "$rt/python" ] || continue
  # only pack COMPLETE runtimes — a partial install can leave a python/ dir with
  # no open_webui; never ship that. open_webui lives under one of these layouts:
  #   nix-native venv : venv/lib/pythonX.Y/site-packages   (nixos)
  #   unix pbs        : python/lib/pythonX.Y/site-packages (linux/mac)
  #   windows pbs     : python/Lib/site-packages           (win)
  if ! glob_exists "$rt/venv/lib/python*/site-packages/open_webui/main.py" \
     && ! glob_exists "$rt/python/lib/python*/site-packages/open_webui/main.py" \
     && [ ! -f "$rt/python/Lib/site-packages/open_webui/main.py" ]; then
    warn "skip incomplete runtime $t (open_webui missing)"; continue
  fi
  emit "$rt" "runtime-$t" $(fmts_for "runtime-$t")
  RUNTIMES="$(jq -c --arg t "$t" '. + [$t]' <<<"$RUNTIMES")"
done

# ollama: extracted to a dir, then emitted in the per-OS format. Prefer the Nix
# no-fixup repack (cached, binaries byte-identical, copied OUT of the store);
# fall back to the locally downloaded flavour archive.
OLLAMAS="[]"
extract_to() {  # <archive> <destdir>
  local src="$1" d="$2"; mkdir -p "$d"
  case "$src" in
    *.tar.zst) need zstd; zstd -dc "$src" | tar -x -C "$d" ;;
    *.tar.gz|*.tgz) tar -xzf "$src" -C "$d" ;;
    *.zip) need unzip; unzip -qo "$src" -d "$d" ;;
    *) return 1 ;;
  esac
}

NIXOLL=""
if command -v nix >/dev/null 2>&1 && [ -f "$REPO_ROOT/vendor.lock.json" ]; then
  log "nix build .#ollamaComponents (cached no-fixup repack)"
  NIXOLL="$(nix build "$REPO_ROOT#ollamaComponents" --no-link --print-out-paths 2>/dev/null || true)"
fi
for key in linux-amd64 linux-arm64 linux-amd64-rocm darwin windows-amd64; do
  tmp="$(mktemp -d)"; got=""
  if [ -n "$NIXOLL" ] && [ -f "$NIXOLL/ollama-$key.tar.gz" ]; then
    extract_to "$NIXOLL/ollama-$key.tar.gz" "$tmp" && got=nix
  fi
  if [ -z "$got" ]; then
    for ext in tar.zst tgz zip; do
      f="$OLLAMA_DIR/ollama-$key.$ext"; [ -f "$f" ] || continue
      extract_to "$f" "$tmp" && got=local; break
    done
  fi
  if [ -n "$got" ]; then
    emit "$tmp" "ollama-$key" $(fmts_for "ollama-$key")
    OLLAMAS="$(jq -c --arg k "$key" '. + [$k]' <<<"$OLLAMAS")"
  fi
  rm -rf "$tmp"
done

jq -n --arg otag "$OLLAMA_TAG" --argjson runtimes "$RUNTIMES" --argjson ollama "$OLLAMAS" \
  '{ollama_tag:$otag, ow_assets:"ow-assets", runtimes:$runtimes, ollama:$ollama,
    note:"loader picks runtime-<this bundles OS> + ollama by CPU arch (rocm if /dev/kfd); mounts .squashfs (else extracts), extracts .tar.gz"}' \
  > "$OUT/manifest.json"

log "components -> $OUT ($(du -sh "$OUT" | cut -f1))"
