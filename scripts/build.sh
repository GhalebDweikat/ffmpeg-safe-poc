#!/bin/bash
# Runs inside the container. Builds the Rust decoders, adds each one to a
# copy of the FFmpeg source as the decoder "NAME_rs", and builds FFmpeg.
# With FFSAFE_WORK=NAME it builds that decoder's own tree instead, which
# starts as a copy of the main one (so build the main one first).
set -euo pipefail
. /proj/scripts/common.sh

echo "== Rust: unit tests and static library ($ROOT)"
cd "$PROJ/rust"
for c in $ALL_CODECS; do
    cargo test --release -p "$c-core" 2>&1 | grep -E "^test .*FAILED|test result|^error|^warning: unused" || true
done
cargo test --release -p ffsafe-ffi $CARGO_FEATURES 2>&1 | grep -E "^test .*FAILED|test result|^error" || true
cargo build --release -p ffsafe-ffi $CARGO_FEATURES 2>&1 | grep -E "^error|^warning|Finished" -A 12 | head -n 60
ls -l "$RUSTLIB"

echo "== FFmpeg: copy source and add the ports"
if [ ! -f "$SRC/configure" ]; then
    if [ -n "${FFSAFE_WORK:-}" ]; then
        [ -x "$BASE/ffmpeg/ffmpeg" ] || { echo "build the main tree first (build.sh without FFSAFE_WORK)"; exit 1; }
        cp -a "$BASE/ffmpeg" "$SRC"
    else
        mkdir -p "$SRC"
        rsync -a --exclude .git "$PROJ/ffmpeg/" "$SRC/"
    fi
fi
link_rustlib "$SRC"
changed=$(add_ports "$SRC" $ALL_CODECS)
grep -c "_rs_decoder" "$SRC/libavcodec/allcodecs.c"

cd "$SRC"
if [ -n "$changed" ] || [ ! -f config.h ] || ! grep -q "$RUSTLIB_REL" ffbuild/config.mak; then
    echo "== FFmpeg: configure"
    ./configure --disable-doc --disable-autodetect \
        --extra-libs="$RUSTLIB_REL $SYSLIBS" | tail -n 4
fi

echo "== FFmpeg: make"
rm -f ffmpeg ffmpeg_g ffprobe ffprobe_g   # relink against the current Rust library
make -j"$JOBS" 2>&1 | grep -E "warning: .*_rs\.c|error|Error|_rs\.c:" || true
ls -l ffmpeg
./ffmpeg -hide_banner -decoders | grep -E " ($(echo $ALL_CODECS | tr ' ' '|'))_rs "
