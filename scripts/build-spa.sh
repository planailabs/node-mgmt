#!/usr/bin/env bash
# Build the Dioxus SPA dashboard (launcher/spa-src) into launcher/spa/ — the
# static web assets the rust launcher embeds (rust-embed) and serves over
# localhost. Uses the flake's `spa` package (reproducible: vendored fork + dx +
# tailwind), so it works the same in dev and CI.
#
#   scripts/build-spa.sh            # nix build .#spa  -> launcher/spa/
#   scripts/build-spa.sh --dev      # in-tree `dx build` (needs `nix develop`)
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

SPA_SRC="$REPO_ROOT/launcher/spa-src"
SPA_OUT="$REPO_ROOT/launcher/spa"

if [ "${1:-}" = "--dev" ]; then
  # In-tree build via the devshell's dx/wasm-bindgen/tailwind (faster iteration).
  need dx
  # `future` (usb.lock) gates the Config tab. Mirror the flake's .#spa build.
  FUTURE_FEAT=""
  if [ "$(jq -r '.future // false' "$REPO_ROOT/usb.lock" 2>/dev/null)" = "true" ]; then
    FUTURE_FEAT="--features future"
  fi
  ( cd "$SPA_SRC" \
    && tailwindcss -i ../../third_party/plan-ai-design/assets/input.css \
         -o assets/tailwind.css --config tailwind.config.js --minify \
    && dx build --platform web --release $FUTURE_FEAT )
  SRC="$SPA_SRC/target/dx/plan-ai-spa/release/web/public"
else
  need nix
  OUT="$(cd "$REPO_ROOT" && nix build .#spa --no-link --print-out-paths)"
  SRC="$OUT"
fi

[ -d "$SRC" ] || die "SPA build produced no output at $SRC"
rm -rf "$SPA_OUT"
mkdir -p "$SPA_OUT"
cp -r "$SRC"/. "$SPA_OUT/"
chmod -R u+w "$SPA_OUT"
log "SPA built into launcher/spa/ ($(du -sh "$SPA_OUT" | cut -f1))"
