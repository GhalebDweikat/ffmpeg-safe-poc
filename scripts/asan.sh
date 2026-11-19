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
ASRC=$ROOT/ffmpeg-asan
SAN="-fsanitize=address,undefined -fno-sanitize-recover=undefined -fno-omit-frame-pointer"

# FFmpeg cannot build out of tree next to a configured tree, so this build
# gets its own copy of the pristine source with the same decoders added.
if [ ! -f "$ASRC/configure" ]; then
    mkdir -p "$ASRC"
    rsync -a --exclude .git "$PROJ/ffmpeg/" "$ASRC/"
fi
link_rustlib "$ASRC"
changed=$(add_ports "$ASRC" $ALL_CODECS)
decoders="" demuxers="" parsers="" extra=""
for c in $ALL_CODECS; do
    load_codec "$c"
    decoders="$decoders,$C_DECODER,${c}_rs"
    demuxers="$demuxers,$DEMUXERS"
    [ -n "$PARSERS" ] && parsers="$parsers,$PARSERS"
    extra="$extra $ASAN_EXTRA"
done
[ -n "$parsers" ] && extra="$extra --enable-parser=${parsers#,}"

cd "$ASRC"
want="$decoders|$demuxers|$extra"
if [ -n "$changed" ] || [ ! -f config.h ] || [ "$(cat .ffsafe-config 2>/dev/null)" != "$want" ]; then
    # configure runs sanitized test programs, and gcc 12's sanitizer runtime
    # sometimes loops printing "DEADLYSIGNAL" before main (see fuzz.sh). So
    # configure gets a time limit and a log size limit, and is tried again.
    rm -f .ffsafe-config config.h
    for attempt in 1 2 3 4 5 6 7 8; do
        if (ulimit -f 400000
            timeout -s KILL 300 ./configure --disable-doc --disable-autodetect --disable-programs \
                --disable-everything --disable-x86asm --disable-stripping \
                --enable-decoder="${decoders#,}" --enable-demuxer="${demuxers#,}" --enable-protocol=file $extra \
                --extra-cflags="$SAN -g" --extra-ldflags="$SAN" \
                --extra-libs="$RUSTLIB_REL $SYSLIBS" >/tmp/asan-configure.$$ 2>&1); then
            tail -n 2 /tmp/asan-configure.$$
            echo "$want" >.ffsafe-config
            break
        fi
        rm -f ffbuild/config.log
        echo "configure attempt $attempt did not finish; trying again"
    done
    rm -f /tmp/asan-configure.$$
    [ -f .ffsafe-config ] || { echo "configure never finished"; exit 1; }
fi
make -j"$JOBS" 2>&1 | grep -E "error|Error" || true

LIBS="libavformat/libavformat.a libavcodec/libavcodec.a libavutil/libavutil.a $RUSTLIB $SYSLIBS"
gcc -O1 -g $SAN -Wall -I"$ASRC" "$PROJ/harness/difffuzz.c" $(harness_sources) \
    $LIBS -o "$HARNESS/difffuzz-asan"
ls -l "$HARNESS/difffuzz-asan"

export ASAN_OPTIONS=detect_leaks=1
bash "$PROJ/scripts/fuzz.sh" "$CODEC_ARG" "$SECS" "$GEN" "$HARNESS/difffuzz-asan"
