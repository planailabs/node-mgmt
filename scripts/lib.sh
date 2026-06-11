#!/usr/bin/env bash
# Shared helpers for plan-ai-usb-minimal scripts.
# Source this file: . "$(dirname "$0")/lib.sh"
set -euo pipefail

# --- paths ------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
USB_LOCK="$REPO_ROOT/usb.lock"
VENDOR_DIR="$REPO_ROOT/vendor"
DIST_DIR="$REPO_ROOT/dist"

# --- logging ----------------------------------------------------------------
log()  { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[err]\033[0m %s\n' "$*" >&2; exit 1; }

# --- prerequisites ----------------------------------------------------------
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1 (enter 'nix develop')"; }

# --- temp output dirs -------------------------------------------------------
# Build each step into a FRESH temp dir, then atomically swap it in. A failed /
# interrupted step then never leaves a partial output for a later step to consume
# ("no stale files just there"); the previous good output stays until the new one
# is complete. The temp lives under DIST_DIR so the final `mv` is same-filesystem
# (atomic). Pair stage_dir at the start with publish_dir at the end.
stage_dir() {  # <name> -> prints a fresh temp build dir under dist/
  mkdir -p "$DIST_DIR"
  mktemp -d "$DIST_DIR/.stage-$1.XXXXXX"
}
publish_dir() {  # <tmp-dir> <final-dir>
  local tmp="$1" final="$2"
  [ -d "$tmp" ] || die "publish_dir: missing staged dir $tmp"
  mkdir -p "$(dirname "$final")"
  rm -rf "$final.prev" 2>/dev/null || true
  [ -e "$final" ] && mv "$final" "$final.prev"
  mv "$tmp" "$final"
  rm -rf "$final.prev" 2>/dev/null || true
}

# --- usb.lock accessors -----------------------------------------------------
# lock <jq-filter> -> value from usb.lock
lock() {
  need jq
  [ -f "$USB_LOCK" ] || die "usb.lock not found at $USB_LOCK"
  jq -er "$1" "$USB_LOCK" 2>/dev/null || die "usb.lock: missing/invalid field for filter: $1"
}

ollama_repo()    { lock '.ollama.repo'; }
ollama_version() { lock '.ollama.version'; }
ow_repo()        { lock '.openwebui.repo'; }
ow_version()     { lock '.openwebui.version'; }
llmfit_repo()    { lock '.llmfit.repo'; }
llmfit_version() { lock '.llmfit.version'; }
hermes_repo()    { lock '.hermes.repo'; }
hermes_version() { lock '.hermes.version'; }
llamacpp_repo()    { lock '.llamacpp.repo'; }
llamacpp_version() { lock '.llamacpp.version'; }
py_version()     { lock '.python'; }
pbs_release()    { lock '.pbs_release'; }

# --- github api -------------------------------------------------------------
# Honour GITHUB_TOKEN to avoid the 60 req/h anonymous rate limit.
gh_curl() {
  local url="$1"; shift
  local auth=()
  [ -n "${GITHUB_TOKEN:-}" ] && auth=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
  curl -fsSL "${auth[@]}" -H "Accept: application/vnd.github+json" "$url" "$@"
}

# release_json <repo> <tag> -> release object JSON on stdout
release_json() {
  need curl
  gh_curl "https://api.github.com/repos/$1/releases/tags/$2"
}

# --- target / platform maps -------------------------------------------------
# python-build-standalone + uv triple for a target.
target_triple() {
  case "$1" in
    linux-x64) echo "x86_64-unknown-linux-gnu" ;;
    linux-arm64) echo "aarch64-unknown-linux-gnu" ;;
    mac-arm64) echo "aarch64-apple-darwin" ;;
    mac-x64)   echo "x86_64-apple-darwin" ;;
    win-x64)   echo "x86_64-pc-windows-msvc" ;;
    *) die "unknown target: $1" ;;
  esac
}

# python-build-standalone install_only tarball URL for a target.
pbs_url() {
  local target="$1" pyver pbs triple
  pyver="$(py_version)"; pbs="$(pbs_release)"; triple="$(target_triple "$target")"
  echo "https://github.com/astral-sh/python-build-standalone/releases/download/${pbs}/cpython-${pyver}+${pbs}-${triple}-install_only.tar.gz"
}

# --- checksums --------------------------------------------------------------
# sha256_of <file> -> bare hex digest
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  else need shasum; shasum -a 256 "$1" | awk '{print $1}'; fi
}

# verify_sha256 <file> <expected-hex>  (returns non-zero on mismatch)
verify_sha256() {
  local got; got="$(sha256_of "$1")"
  [ "$got" = "$2" ] || { warn "sha256 mismatch for $1: got $got want $2"; return 1; }
}

# download_verified <url> <dest> <expected-sha256|->
# Resumable; skips when an existing file already matches the expected digest.
download_verified() {
  need curl
  local url="$1" dest="$2" want="${3:--}"
  mkdir -p "$(dirname "$dest")"
  if [ -f "$dest" ] && [ "$want" != "-" ] && verify_sha256 "$dest" "$want" 2>/dev/null; then
    log "cached  $(basename "$dest")"
    return 0
  fi
  log "fetch   $(basename "$dest")"
  # Try a resumed download; if the server doesn't support byte ranges (curl 33,
  # common when the existing file is already complete), restart from scratch.
  if ! curl -fL --retry 3 -C - -o "$dest" "$url"; then
    warn "resume failed, restarting download: $(basename "$dest")"
    rm -f "$dest"
    curl -fL --retry 3 -o "$dest" "$url"
  fi
  if [ "$want" != "-" ]; then
    verify_sha256 "$dest" "$want" || die "checksum verification failed: $dest"
  fi
}

# --- component packing primitives -------------------------------------------
# Shared by scripts/pack-component.sh (one component per ninja edge) and
# scripts/bundle.sh (the app-<os> component). Each writes to a temp path and
# atomically mv's it into place, so an interrupted pack never leaves a partial
# file for a later step — even though each component is now its own ninja edge.
ncpu() { nproc 2>/dev/null || echo 1; }
# parallel gzip when available (output is plain gzip — the node loader's tar reads it)
gzip_cmd() { if command -v pigz >/dev/null 2>&1; then echo "pigz -p $(ncpu)"; else echo gzip; fi; }

pack_gz() {  # <srcdir> <out.tar.gz>
  local src="$1" out="$2" gz tmp="$2.tmp.$$"; gz="$(gzip_cmd)"
  tar -C "$src" -cf - . | $gz > "$tmp" && mv -f "$tmp" "$out"
}
pack_squashfs() {  # <srcdir> <out.squashfs>
  need mksquashfs; local src="$1" out="$2" tmp="$2.tmp.$$"; rm -f "$tmp"
  mksquashfs "$src" "$tmp" -comp zstd -processors "$(ncpu)" -all-root -no-xattrs -noappend -quiet
  mv -f "$tmp" "$out"
}
pack_dir() {  # <srcdir> <out-dir>   (pre-extracted; used in place on windows/FAT32)
  local src="$1" out="$2" tmp="$2.tmp.$$"
  rm -rf "$tmp"; mkdir -p "$tmp"; cp -a "$src/." "$tmp/"; rm -rf "$out"; mv -f "$tmp" "$out"
}
# Windows component delivery format: ONE .zip the launcher downloads + unpacks on
# update (vs thousands of individually-tracked files). The burned image carries it
# UNPACKED — make-usb-image.sh expands + removes the zip after the manifest is built.
pack_zip() {  # <srcdir> <out.zip>
  need zip; local src="$1" out="$2" tmp
  # Resolve out to ABSOLUTE before the `cd "$src"` below (a relative out — e.g. from
  # ninja — would otherwise be created relative to src and fail).
  mkdir -p "$(dirname "$out")"; out="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"
  tmp="$out.tmp.$$"; rm -f "$tmp"
  # Deterministic-ish: add entries in sorted order, drop extra file attributes (-X).
  # Follows symlinks (stores their content) so the archive unpacks cleanly on Windows.
  ( cd "$src" && find . -mindepth 1 | LC_ALL=C sort | zip -q -X -@ "$tmp" )
  mv -f "$tmp" "$out"
}
# raw HFS+ image macOS mounts via hdiutil, compressed to a UDIF dmg (Finder-
# mountable, ~3x smaller) via libdmg-hfsplus. Needs mkfs.hfsplus (hfsprogs) + a
# sudo loop-mount. Returns non-zero ONLY when no dmg could be produced (no
# mkfs.hfsplus / mount failed) so the caller can fall back; a bare-HFS+ result
# (libdmg unavailable) still counts as success.
pack_dmg() {  # <srcdir> <out.dmg> [volume-label]
  command -v mkfs.hfsplus >/dev/null 2>&1 || { warn "no mkfs.hfsplus — skip $(basename "$2")"; return 1; }
  local src="$1" img="$2" vol="${3:-PlanAI}" mnt sz raw kb nfiles
  raw="$(mktemp -u).rawhfs"
  # Size with `du -l` so EACH hard-link name counts (the cp below breaks hard links
  # into copies — see why there) plus a per-file pad for the catalog B-tree and a
  # fixed slack. Apparent size (du -sb) badly under-counts overhead for a runtime's
  # ~57k mostly-tiny files, so a tight estimate ENOSPCs the cp mid-copy. Over-
  # provisioning costs nothing: libdmg compresses the empty space out of the UDIF.
  kb=$(du -slk "$src" | cut -f1); nfiles=$(find "$src" | wc -l)
  sz=$(( kb * 1024 + nfiles * 4096 + 256*1024*1024 ))
  truncate -s "$sz" "$raw"
  mkfs.hfsplus -v "$vol" "$raw" >/dev/null 2>&1 || { warn "mkfs.hfsplus failed: $(basename "$img")"; rm -f "$raw"; return 1; }
  mnt="$(mktemp -d)"
  if ! sudo mount -o loop,umask=0000 "$raw" "$mnt" 2>/dev/null; then
    warn "loop-mount failed for $(basename "$img") (sudo?)"; rmdir "$mnt"; rm -f "$raw"; return 1; fi
  # --no-preserve=links: copy hard-linked files as independent copies. The Linux
  # HFS+ driver can't create hard links ("Operation not permitted"), and a runtime
  # has thousands (uv/pip dedupes identical wheel metadata across packages). Symlinks
  # are still preserved (cp -a's -d stays). The duplication is cheap (~60 MB) and the
  # UDIF compresses it back out.
  if ! sudo cp -a --no-preserve=links "$src/." "$mnt/"; then
    warn "cp into dmg failed: $(basename "$img")"; sudo umount "$mnt" 2>/dev/null || true; rmdir "$mnt" 2>/dev/null || true; rm -f "$raw"; return 1; fi
  sync; sudo umount "$mnt"; rmdir "$mnt" 2>/dev/null || true
  rm -f "$img"
  local DMGTOOL; DMGTOOL="$(cd "$REPO_ROOT" && nix build .#libdmg-hfsplus --no-link --print-out-paths 2>/dev/null)/bin/dmg"
  if [ -x "$DMGTOOL" ] && "$DMGTOOL" dmg "$raw" "$img" >/dev/null 2>&1; then
    rm -f "$raw"; return 0
  fi
  warn "libdmg-hfsplus unavailable — bare HFS+ dmg for $(basename "$img")"; mv "$raw" "$img"; return 0
}

# True if a glob matches at least one existing path. Portable substitute for
# `compgen -G` (unavailable in the non-interactive bash `nix develop` provides):
# relies on an unmatched glob staying literal (no nullglob).
glob_exists() { local m; for m in $1; do [ -e "$m" ] && return 0; done; return 1; }

extract_to() {  # <archive> <destdir>
  local src="$1" d="$2"; mkdir -p "$d"
  # Resolve symlinks (vendored archives point into /nix/store): zstd refuses to
  # read a symlink input, and following it is harmless for a real file too.
  src="$(readlink -f "$src")"
  case "$src" in
    *.tar.zst) need zstd; zstd -dc "$src" | tar -x -C "$d" ;;
    *.tar.gz|*.tgz) tar -xzf "$src" -C "$d" ;;
    *.zip) need unzip; unzip -qo "$src" -d "$d" ;;
    *) return 1 ;;
  esac
}
