#!/bin/bash
# Runs inside the container. Builds the fuzz and benchmark harnesses against
# the FFmpeg build tree, and makes each codec's test clips.
#   harness.sh [build-only]
set -euo pipefail
. /proj/scripts/common.sh

LIBS="$SRC/libavformat/libavformat.a $SRC/libavcodec/libavcodec.a $SRC/libswresample/libswresample.a $SRC/libavutil/libavutil.a $RUSTLIB $SYSLIBS"
mkdir -p "$HARNESS"
for h in difffuzz bench; do
    gcc -O2 -Wall -I"$SRC" "$PROJ/harness/$h.c" "$PROJ/harness/common.c" "$PROJ"/harness/codec_*.c $LIBS -o "$HARNESS/$h"
done
[ "${1:-}" = build-only ] && exit 0
ls -l "$HARNESS"

ff="$SRC/ffmpeg -hide_banner -loglevel error -y"
enc() { # output name, ffmpeg input and encoder arguments...
    local out="$CLIPS/$CODEC/$1"; shift
    [ -s "$out" ] || $ff "$@" "$out" || { rm -f "$out"; echo "could not encode $out"; }
}
real() { # FATE sample path, output name
    [ -s "$CLIPS/$CODEC/$2" ] || cp "$SAMPLES/$1" "$CLIPS/$CODEC/$2"
}
for c in $ALL_CODECS; do
    load_codec "$c"
    mkdir -p "$CLIPS/$c"
    make_clips
    echo "== $c clips"
    ls -l "$CLIPS/$c" | awk 'NR > 1 { print $5, $9 }'
done
