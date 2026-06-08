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
