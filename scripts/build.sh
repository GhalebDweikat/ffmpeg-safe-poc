#!/bin/bash
# Runs inside the container. Builds the Rust decoders, adds each one to a
# copy of the FFmpeg source as the decoder "NAME_rs", and builds FFmpeg.
set -euo pipefail
. /proj/scripts/common.sh

echo "== Rust: unit tests and static library"
cd "$PROJ/rust"
for c in $ALL_CODECS; do
    cargo test --release -p "$c-core" 2>&1 | grep -E "^test .*FAILED|test result|^error|^warning: unused" || true
done
cargo test --release -p ffsafe-ffi 2>&1 | grep -E "^test .*FAILED|test result|^error" || true
cargo build --release -p ffsafe-ffi 2>&1 | tail -n 3
ls -l "$RUSTLIB"

echo "== FFmpeg: copy source and add the ports"
if [ ! -f "$SRC/configure" ]; then
    mkdir -p "$SRC"
    rsync -a --exclude .git "$PROJ/ffmpeg/" "$SRC/"
fi
changed=$(add_ports "$SRC")
grep -n "_rs_decoder" "$SRC/libavcodec/allcodecs.c"

cd "$SRC"
if [ -n "$changed" ] || [ ! -f config.h ] || ! grep -q "$RUSTLIB" ffbuild/config.mak; then
    echo "== FFmpeg: configure"
    ./configure --disable-doc --disable-autodetect \
        --extra-libs="$RUSTLIB $SYSLIBS" | tail -n 4
fi

echo "== FFmpeg: make"
rm -f ffmpeg ffmpeg_g ffprobe ffprobe_g   # relink against the current Rust library
make -j"$(nproc)" 2>&1 | grep -E "warning: .*_rs\.c|error|Error|_rs\.c:" || true
ls -l ffmpeg
./ffmpeg -hide_banner -decoders | grep -E "_rs "
