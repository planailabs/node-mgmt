#!/usr/bin/env bash
# Split a large file (the linux AppImage) into <4 GiB parts so it fits on FAT32,
# and emit a self-reassembling launcher.
#
# Usage: scripts/split-appimage.sh [file] [--out DIR] [--chunk-mb N]
#   file      : default = newest dist/bundle/*.AppImage
#   --out     : output dir (default = alongside the file)
#   --chunk-mb: part size in MB (default 3900, safely under FAT32's 4 GiB limit)
#
# Produces, for <name>.AppImage:
#   <name>.AppImage.part00, .part01, …   (each < 4 GiB)
#   <name>.AppImage.sha256               (checksum of the whole)
#   <name>.run.sh                        (cat parts -> cache, verify, exec)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

FILE=""; OUT=""; CHUNK_MB=3900
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    --chunk-mb) CHUNK_MB="$2"; shift 2 ;;
    -*) die "unknown flag: $1" ;;
    *) FILE="$1"; shift ;;
  esac
done
[ -n "$FILE" ] || FILE="$(ls -t "$DIST_DIR"/bundle/*.AppImage 2>/dev/null | head -1 || true)"
[ -n "$FILE" ] && [ -f "$FILE" ] || die "no AppImage found (build it or pass a path)"
OUT="${OUT:-$(dirname "$FILE")}"; mkdir -p "$OUT"

NAME="$(basename "$FILE")"
PREFIX="$OUT/$NAME.part"
SUM="$(sha256_of "$FILE")"
SIZE=$(stat -c%s "$FILE")

log "split $NAME ($((SIZE/1024/1024))MB) into ${CHUNK_MB}MB parts -> $OUT"
rm -f "$OUT/$NAME.part"* "$OUT/$NAME.sha256"
# numeric suffixes: .part00, .part01, …
split -b "${CHUNK_MB}M" -d -a 2 "$FILE" "$PREFIX"
echo "$SUM  $NAME" > "$OUT/$NAME.sha256"

# guard: every part must fit FAT32
FOURGIB=$((4*1024*1024*1024 - 1))
N=0
for p in "$PREFIX"*; do
  sz=$(stat -c%s "$p"); N=$((N+1))
  [ "$sz" -le "$FOURGIB" ] || die "part $(basename "$p") still exceeds 4 GiB"
  log "  $(basename "$p")  $((sz/1024/1024))MB"
done

RUN="$OUT/${NAME%.AppImage}.run.sh"
cat > "$RUN" <<EOF
#!/bin/sh
# Reassemble + launch $NAME (split for FAT32). Parts are joined into a cache dir
# (FAT32 can't hold the >4 GiB whole), verified, then run.
set -e
here=\$(CDPATH= cd -- "\$(dirname -- "\$0")" && pwd)
cache="\${PLANAI_CACHE:-\${XDG_CACHE_HOME:-\$HOME/.cache}/plan-ai}"
mkdir -p "\$cache"
app="\$cache/$NAME"
want="$SUM"
have=""
[ -f "\$app" ] && have=\$( (sha256sum "\$app" 2>/dev/null || shasum -a 256 "\$app") | awk '{print \$1}')
if [ "\$have" != "\$want" ]; then
  echo "reassembling $NAME into \$app ..." >&2
  cat "\$here/$NAME.part"* > "\$app"
  got=\$( (sha256sum "\$app" 2>/dev/null || shasum -a 256 "\$app") | awk '{print \$1}')
  [ "\$got" = "\$want" ] || { echo "checksum mismatch (\$got != \$want)" >&2; exit 1; }
  chmod +x "\$app"
fi
exec "\$app" "\$@"
EOF
chmod +x "$RUN"

log "wrote launcher: $(basename "$RUN")  ($N parts; checksum $NAME.sha256)"
log "on FAT32: copy the .part* files + $(basename "$RUN"), then run $(basename "$RUN")"
