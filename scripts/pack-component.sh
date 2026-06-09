#!/usr/bin/env bash
# Pack ONE modular component into dist/components/. Invoked once per component by
# ninja (see xtask `write_ninja`), so the independent squashfs/dmg/dir packs run
# in parallel and each is staleness-tracked on its own — replacing the old
# monolithic build-components.sh + its hand-rolled `wait -n` job pool. The
# manifest is written separately by `xtask components-manifest` once every
# component is packed.
#
# Source + per-OS format by component name (the loader prefers mount over copy):
#   ow-assets         vendor/ow-assets        -> squashfs + dmg + dir (shared by all OSes)
#   runtime-<target>  dist/runtime/<target>   -> linux=squashfs mac=dmg win=dir
#   ollama-<key>      nix repack / vendor arch -> linux=squashfs mac=dmg win=dir
# A missing/incomplete source is skipped (warn, exit 0); the manifest then omits it.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

NAME="${1:?usage: pack-component.sh <component-name>}"
OUT="$DIST_DIR/components"; mkdir -p "$OUT"

# which formats a component ships in (drives the per-OS bundle staging):
#   linux/nixos = squashfs (mounted/extracted) ; macOS = dmg (hdiutil mount) ;
#   windows = pre-extracted dir (used in place). No .tar.gz is produced anymore.
fmts_for() { case "$1" in
  *linux-*|*nixos-*) echo sqfs ;;
  *windows-*|*win-*) echo dir ;;
  *darwin*|*mac-*)   echo dmg ;;
  ow-assets)         echo "dir" ;;           # win(dir) only; linux squashfs + mac dmg are built in nix (import-build-component.sh)
  *)                 echo gz ;;
esac; }

# emit a directory as one or more component formats (fmt in gz|sqfs|dmg|dir).
emit() { local dir="$1" name="$2"; shift 2; local f
  for f in "$@"; do case "$f" in
    gz)   pack_gz "$dir" "$OUT/$name.tar.gz";   log "+ $name.tar.gz ($(du -h "$OUT/$name.tar.gz" | cut -f1))" ;;
    sqfs) pack_squashfs "$dir" "$OUT/$name.squashfs"; log "+ $name.squashfs ($(du -h "$OUT/$name.squashfs" | cut -f1)) [squashfs]" ;;
    dmg)  if pack_dmg "$dir" "$OUT/$name.dmg"; then log "+ $name.dmg ($(du -h "$OUT/$name.dmg" | cut -f1))"
          else [ -f "$OUT/$name.tar.gz" ] || { pack_gz "$dir" "$OUT/$name.tar.gz"; log "+ $name.tar.gz (dmg fallback)"; }; fi ;;
    dir)  pack_dir "$dir" "$OUT/$name"; log "+ $name/ (dir, $(du -sh "$OUT/$name" | cut -f1))" ;;
  esac; done; }

case "$NAME" in
  # shared offline assets (linux sqfs + mac dmg + win dir)
  ow-assets)
    [ -d "$VENDOR_DIR/ow-assets" ] || { warn "no vendor/ow-assets — skip ow-assets"; exit 0; }
    emit "$VENDOR_DIR/ow-assets" ow-assets $(fmts_for ow-assets) ;;

  # a built python runtime. open_webui lives under one of:
  #   nix-native venv : venv/lib/pythonX.Y/site-packages   (nixos)
  #   unix pbs        : python/lib/pythonX.Y/site-packages (linux/mac)
  #   windows pbs     : python/Lib/site-packages           (win)
  runtime-*)
    t="${NAME#runtime-}"; rt="$DIST_DIR/runtime/$t"
    [ -d "$rt/venv" ] || [ -d "$rt/python" ] || { warn "no runtime $t — skip"; exit 0; }
    if ! glob_exists "$rt/venv/lib/python*/site-packages/open_webui/main.py" \
       && ! glob_exists "$rt/python/lib/python*/site-packages/open_webui/main.py" \
       && [ ! -f "$rt/python/Lib/site-packages/open_webui/main.py" ]; then
      warn "skip incomplete runtime $t (open_webui missing)"; exit 0; fi
    emit "$rt" "runtime-$t" $(fmts_for "runtime-$t") ;;

  # an ollama flavour: prefer the Nix no-fixup repack (cached, binaries byte-
  # identical, copied OUT of the store) else the locally-vendored archive, then
  # pack it. Extraction is into a private temp dir, removed on exit.
  ollama-*)
    key="${NAME#ollama-}"
    OLLAMA_DIR="$VENDOR_DIR/ollama/$(ollama_version)"
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
    got=""; NIXOLL=""
    if command -v nix >/dev/null 2>&1 && [ -f "$REPO_ROOT/vendor.lock.json" ]; then
      NIXOLL="$(nix build "$REPO_ROOT#ollamaComponents" --no-link --print-out-paths 2>/dev/null || true)"
    fi
    if [ -n "$NIXOLL" ] && [ -f "$NIXOLL/ollama-$key.tar.gz" ]; then
      extract_to "$NIXOLL/ollama-$key.tar.gz" "$tmp" && got=nix
    fi
    if [ -z "$got" ]; then
      for ext in tar.zst tgz zip; do
        f="$OLLAMA_DIR/ollama-$key.$ext"; [ -f "$f" ] || continue
        extract_to "$f" "$tmp" && got=local; break
      done
    fi
    [ -n "$got" ] || { warn "no ollama-$key archive — skip"; exit 0; }
    emit "$tmp" "ollama-$key" $(fmts_for "ollama-$key") ;;

  *) die "unknown component: $NAME" ;;
esac
