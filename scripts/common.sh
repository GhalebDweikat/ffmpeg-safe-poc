# Sourced by the other scripts (all of which run inside the container).
#
# Environment:
#   FFSAFE_WORK=NAME  work on one decoder in its own build tree
#                     (/build/w/NAME), so that several decoders can be
#                     ported at the same time without sharing a build
#   FFSAFE_JOBS=N     parallel make jobs (default: all CPUs)
#   FFSAFE_CPU=N      CPU the benchmarks are pinned to (default 6)
PROJ=/proj
BASE=/build
ROOT=$BASE
# Every ported codec has a description in scripts/codecs/NAME.sh.
EVERY_CODEC=$(cd "$PROJ/scripts/codecs" && ls ./*.sh | sed 's|^\./||; s|\.sh$||' | tr '\n' ' ')
ALL_CODECS=$EVERY_CODEC
CARGO_FEATURES=""
if [ -n "${FFSAFE_WORK:-}" ]; then
    [ -f "$PROJ/scripts/codecs/$FFSAFE_WORK.sh" ] || { echo "FFSAFE_WORK: unknown codec $FFSAFE_WORK"; exit 3; }
    ROOT=$BASE/w/$FFSAFE_WORK
    ALL_CODECS=$FFSAFE_WORK
    # Build this decoder and the finished ones; leave out the others that
    # are still being worked on (STATUS=wip in their description).
    feat=$FFSAFE_WORK
    for c in $EVERY_CODEC; do
        [ "$c" = "$FFSAFE_WORK" ] && continue
        grep -q '^STATUS=wip' "$PROJ/scripts/codecs/$c.sh" || feat="$feat,$c"
    done
    CARGO_FEATURES="--no-default-features --features $feat"
    mkdir -p "$ROOT"
else
    # The main tree holds the finished decoders only.
    ALL_CODECS="" feat=""
    for c in $EVERY_CODEC; do
        grep -q '^STATUS=wip' "$PROJ/scripts/codecs/$c.sh" && continue
        ALL_CODECS="$ALL_CODECS $c"
        feat="$feat,$c"
    done
    ALL_CODECS=${ALL_CODECS# }
    CARGO_FEATURES="--no-default-features --features ${feat#,}"
fi
SRC=$ROOT/ffmpeg
SAMPLES=$BASE/samples
CLIPS=$ROOT/clips
HARNESS=$ROOT/harness
export CARGO_TARGET_DIR=$ROOT/rust-target
RUSTLIB=$CARGO_TARGET_DIR/release/libffsafe_ffi.a
# FFmpeg is configured with this path, relative to its build tree, so that
# every tree has the same configuration; LIBLINK is a symlink to wherever
# this tree's Rust library is built.
LIBLINK=ffsafe-lib
RUSTLIB_REL=$LIBLINK/libffsafe_ffi.a
SYSLIBS="-lgcc_s -lutil -lrt -lpthread -lm -ldl"
JOBS=${FFSAFE_JOBS:-$(nproc)}
BENCH_CPU=${FFSAFE_CPU:-6}
# The harness sources for the codecs of this build.
harness_sources() {
    local c
    echo "$PROJ/harness/common.c"
    for c in $ALL_CODECS; do
        [ -f "$PROJ/harness/codec_$c.c" ] && echo "$PROJ/harness/codec_$c.c"
    done
}

load_codec() { # name -> sets CODEC and the variables from its description
    CODEC=$1
    SYNTH_SIZE=1280x720 SYNTH_FRAMES=60 FATE_REAL="" FATE_ENC="" FATE_SAMPLES="" FUZZ_SEEDS="" MUTANTS=()
    DEMUXERS="mov,avi" PARSERS="" ASAN_EXTRA=""
    make_clips() { :; }
    [ -f "$PROJ/scripts/codecs/$1.sh" ] || { echo "unknown codec $1 (have: $EVERY_CODEC)"; exit 3; }
    . "$PROJ/scripts/codecs/$1.sh"
}

# Add ports to an FFmpeg source tree: copy the shim, list the decoder ahead
# of the C one (so it is the default for its codec id), add the object
# file. Prints "changed" if the tree needs configure again.
add_ports() { # source tree, codec names...
    local tree=$1 c
    shift
    for c in "$@"; do
        load_codec "$c"
        cmp -s "$PROJ/shim/${c}_rs.c" "$tree/libavcodec/${c}_rs.c" ||
            cp "$PROJ/shim/${c}_rs.c" "$tree/libavcodec/${c}_rs.c"
        if ! grep -q "ff_${c}_rs_decoder" "$tree/libavcodec/allcodecs.c"; then
            sed -i "s/^extern const FFCodec ff_${C_DECODER}_decoder;/extern const FFCodec ff_${c}_rs_decoder;\n&/" "$tree/libavcodec/allcodecs.c"
            sed -i "0,/^OBJS-\$(CONFIG_${CONFIG}_DECODER) .*/s//OBJS-\$(CONFIG_${CONFIG}_RS_DECODER) += ${c}_rs.o\n&/" "$tree/libavcodec/Makefile"
            grep -q "ff_${c}_rs_decoder" "$tree/libavcodec/allcodecs.c" && grep -q "${c}_rs.o" "$tree/libavcodec/Makefile" ||
                { echo "could not register ${c}_rs" >&2; exit 1; }
            echo changed
        fi
    done
}

# Point a tree's library symlink at this build's Rust library.
link_rustlib() { # source tree
    ln -sfn "$CARGO_TARGET_DIR/release" "$1/$LIBLINK"
}
