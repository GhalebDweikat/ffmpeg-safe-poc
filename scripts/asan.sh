#!/bin/bash
# Runs inside the container. Builds a second, minimal FFmpeg with
# AddressSanitizer and UndefinedBehaviorSanitizer on all the C code (the
# original decoders, the shims, libavcodec's frame handling), links the same
# Rust library, and runs the differential fuzzer for one codec under it.
#   asan.sh CODEC [SECS] [GEN]
set -euo pipefail
. /proj/scripts/common.sh

CODEC_ARG=${1:?codec}
SECS=${2:-90}
GEN=${3:-6}
ASRC=/build/ffmpeg-asan
SAN="-fsanitize=address,undefined -fno-sanitize-recover=undefined -fno-omit-frame-pointer"

# FFmpeg cannot build out of tree next to a configured tree, so this build
# gets its own copy of the pristine source with the same decoders added.
if [ ! -f "$ASRC/configure" ]; then
    mkdir -p "$ASRC"
    rsync -a --exclude .git "$PROJ/ffmpeg/" "$ASRC/"
fi
changed=$(add_ports "$ASRC")
decoders=""
for c in $ALL_CODECS; do
    load_codec "$c"
    decoders="$decoders,$C_DECODER,${c}_rs"
done

cd "$ASRC"
if [ -n "$changed" ] || [ ! -f config.h ] || ! grep -q "$RUSTLIB" ffbuild/config.mak; then
    ./configure --disable-doc --disable-autodetect --disable-programs \
        --disable-everything --disable-x86asm --disable-stripping \
        --enable-decoder="${decoders#,}" --enable-demuxer=mov,avi --enable-protocol=file \
        --extra-cflags="$SAN -g" --extra-ldflags="$SAN" \
        --extra-libs="$RUSTLIB $SYSLIBS" | tail -n 2
fi
make -j8 2>&1 | grep -E "error|Error" || true

LIBS="libavformat/libavformat.a libavcodec/libavcodec.a libavutil/libavutil.a $RUSTLIB $SYSLIBS"
gcc -O1 -g $SAN -Wall -I"$ASRC" "$PROJ/harness/difffuzz.c" "$PROJ/harness/common.c" "$PROJ"/harness/codec_*.c \
    $LIBS -o "$HARNESS/difffuzz-asan"
ls -l "$HARNESS/difffuzz-asan"

export ASAN_OPTIONS=detect_leaks=1
bash "$PROJ/scripts/fuzz.sh" "$CODEC_ARG" "$SECS" "$GEN" "$HARNESS/difffuzz-asan"
