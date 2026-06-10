#!/usr/bin/env bash
# Bump every external dependency in usb.lock to its latest upstream release, then
# regenerate the derived locks. Sources of truth:
#
#   ollama       latest github release of .ollama.repo      (ollama/ollama)
#   open-webui   latest github release of .openwebui.repo   (open-webui/open-webui)
#   llmfit       latest github release of .llmfit.repo       (AlexsJones/llmfit)
#   pbs_release  latest astral-sh/python-build-standalone release (a YYYYMMDD tag)
#   python       newest CPython in that pbs release within the supported minor
#                (open-webui needs >=3.11,<3.13 → default minor 3.12; override
#                with PYTHON_MINOR=3.x)
#
# After bumping usb.lock it runs scripts/update-locks.sh (vendor.lock.json +
# uv.lock + per-target wheel locks + app/package-lock.json) unless --no-locks.
#
# Usage:
#   scripts/update-deps.sh [--dry-run] [--no-locks] [--only a,b,c]
#     --dry-run    show what would change; touch nothing
#     --no-locks   bump usb.lock only; skip update-locks.sh
#     --only LIST  restrict to a comma/space list of: ollama openwebui llmfit pbs
#   PYTHON_MINOR=3.12   pin the CPython minor series (default 3.12)
#   GITHUB_TOKEN=...    avoid the 60 req/h anonymous github rate limit
#
# Run inside `nix develop` (needs jq, curl; update-locks also needs uv + npm).
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

need jq; need curl

DRY_RUN=0; RUN_LOCKS=1; ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --no-locks) RUN_LOCKS=0 ;;
    --only) ONLY="${2:-}"; shift ;;
    --only=*) ONLY="${1#--only=}" ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) die "unknown arg: $1 (see --help)" ;;
  esac
  shift
done
PYTHON_MINOR="${PYTHON_MINOR:-3.12}"

# want <name> -> 0 if this dep is in scope (no --only, or listed)
want() { [ -z "$ONLY" ] && return 0; case " ${ONLY//,/ } " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# latest_release_tag <owner/repo> -> the tag_name of the latest (non-prerelease) release
latest_release_tag() {
  gh_curl "https://api.github.com/repos/$1/releases/latest" | jq -er '.tag_name' \
    || die "could not resolve latest release for $1"
}

step() { printf '\033[1;36m▶ %s\033[0m\n' "$*" >&2; }
note() { printf '  %s\n' "$*" >&2; }

# Collect the new values first (read-only) so --dry-run can report without writing.
declare -A NEW
OLLAMA_REPO="$(ollama_repo)"; OW_REPO="$(ow_repo)"; LLMFIT_REPO="$(llmfit_repo)"

if want ollama; then
  step "ollama — latest of $OLLAMA_REPO"; NEW[ollama]="$(latest_release_tag "$OLLAMA_REPO")"
  note "$(ollama_version) -> ${NEW[ollama]}"
fi
if want openwebui; then
  step "open-webui — latest of $OW_REPO"; NEW[openwebui]="$(latest_release_tag "$OW_REPO")"
  note "$(ow_version) -> ${NEW[openwebui]}"
fi
if want llmfit; then
  step "llmfit — latest of $LLMFIT_REPO"; NEW[llmfit]="$(latest_release_tag "$LLMFIT_REPO")"
  note "$(llmfit_version) -> ${NEW[llmfit]}"
fi
if want pbs; then
  step "python-build-standalone — latest release + newest CPython ${PYTHON_MINOR}.x"
  PBS_REL_JSON="$(gh_curl https://api.github.com/repos/astral-sh/python-build-standalone/releases/latest)"
  NEW[pbs]="$(printf '%s' "$PBS_REL_JSON" | jq -er '.tag_name')" || die "no pbs release tag"
  # Newest CPython patch in the supported minor, parsed from the install_only asset names:
  #   cpython-<X.Y.Z>+<pbs>-<triple>-install_only.tar.gz
  NEW[python]="$(printf '%s' "$PBS_REL_JSON" | jq -r --arg m "$PYTHON_MINOR" '
    [ .assets[].name
      | capture("cpython-(?<v>" + ($m|gsub("\\.";"\\.")) + "\\.[0-9]+)\\+") .v ]
    | unique | .[]' | sort -V | tail -1)"
  [ -n "${NEW[python]}" ] || die "no CPython ${PYTHON_MINOR}.x in pbs release ${NEW[pbs]} (try PYTHON_MINOR=…)"
  note "pbs $(pbs_release) -> ${NEW[pbs]}    python $(py_version) -> ${NEW[python]}"
fi

if [ "$DRY_RUN" = 1 ]; then
  log "dry-run — usb.lock not modified"
  exit 0
fi

# Apply to usb.lock atomically (one jq pass, temp + mv).
step "writing usb.lock"
tmp="$(mktemp)"
jq \
  --arg ollama   "${NEW[ollama]:-$(ollama_version)}" \
  --arg ow       "${NEW[openwebui]:-$(ow_version)}" \
  --arg llmfit   "${NEW[llmfit]:-$(llmfit_version)}" \
  --arg python   "${NEW[python]:-$(py_version)}" \
  --arg pbs      "${NEW[pbs]:-$(pbs_release)}" \
  '.ollama.version = $ollama
   | .openwebui.version = $ow
   | .llmfit.version = $llmfit
   | .python = $python
   | .pbs_release = $pbs' \
  "$USB_LOCK" > "$tmp"
mv "$tmp" "$USB_LOCK"
log "usb.lock updated:"
jq -r '"  ollama \(.ollama.version)  open-webui \(.openwebui.version)  llmfit \(.llmfit.version)  python \(.python)  pbs \(.pbs_release)"' "$USB_LOCK"

if [ "$RUN_LOCKS" = 1 ]; then
  step "regenerating derived locks (scripts/update-locks.sh)"
  "$REPO_ROOT/scripts/update-locks.sh"
else
  log "skipped lock regen (--no-locks); run: make update"
fi
log "done — review & commit usb.lock + the regenerated lock files"
