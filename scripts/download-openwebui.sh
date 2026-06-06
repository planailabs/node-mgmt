#!/usr/bin/env bash
# Download Open-WebUI source for the version pinned in usb.lock and extract it.
# Open-WebUI publishes source-only releases (no binaries), so we fetch the tag
# archive, pin/verify it against the tag's commit, and extract to a stable dir.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

REPO="$(ow_repo)"
TAG="$(ow_version)"
OUT="$VENDOR_DIR/open-webui/$TAG"
SRC="$OUT/src"
TARBALL="$OUT/source.tar.gz"
mkdir -p "$OUT"

log "open-webui $REPO@$TAG -> $SRC"

# Record the commit the tag points to (integrity anchor; archives aren't
# checksummed by the API, but the resolved commit is immutable).
need git
COMMIT="$(git ls-remote "https://github.com/$REPO.git" "refs/tags/$TAG^{}" | awk '{print $1}')"
[ -n "$COMMIT" ] || COMMIT="$(git ls-remote "https://github.com/$REPO.git" "refs/tags/$TAG" | awk '{print $1}')"
[ -n "$COMMIT" ] || die "could not resolve commit for $REPO@$TAG"
echo "$COMMIT" > "$OUT/COMMIT"
log "tag $TAG -> commit $COMMIT"

# The /archive/refs/tags/<tag>.tar.gz path has a predictable top-level dir
# (<repo>-<version-without-v>) unlike the API tarball_url (hashed dir).
download_verified "https://github.com/$REPO/archive/refs/tags/$TAG.tar.gz" "$TARBALL" "-"

rm -rf "$SRC"
mkdir -p "$SRC"
# Strip the single top-level directory from the archive.
need tar
tar -xzf "$TARBALL" -C "$SRC" --strip-components=1

[ -f "$SRC/package.json" ] || die "extract looks wrong: $SRC/package.json missing"
[ -f "$SRC/pyproject.toml" ] || die "extract looks wrong: $SRC/pyproject.toml missing"
log "open-webui source extracted -> $SRC"
