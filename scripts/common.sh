# Sourced by the other scripts (all of which run inside the container).
PROJ=/proj
SRC=/build/ffmpeg
SAMPLES=/build/samples
CLIPS=/build/clips
HARNESS=/build/harness
export CARGO_TARGET_DIR=/build/rust-target
RUSTLIB=$CARGO_TARGET_DIR/release/libffsafe_ffi.a
SYSLIBS="-lgcc_s -lutil -lrt -lpthread -lm -ldl"
# Every ported codec has a description in scripts/codecs/NAME.sh.
ALL_CODECS=$(cd "$PROJ/scripts/codecs" && ls ./*.sh | sed 's|^\./||; s|\.sh$||' | tr '\n' ' ')

load_codec() { # name -> sets CODEC and the variables from its description
    CODEC=$1
    SYNTH_SIZE=1280x720 SYNTH_FRAMES=60 FATE_REAL="" FATE_ENC="" FATE_SAMPLES="" FUZZ_SEEDS="" MUTANTS=()
    make_clips() { :; }
    [ -f "$PROJ/scripts/codecs/$1.sh" ] || { echo "unknown codec $1 (have: $ALL_CODECS)"; exit 3; }
    . "$PROJ/scripts/codecs/$1.sh"
}

# Add every port to an FFmpeg source tree: copy the shim, list the decoder
# ahead of the C one (so it is the default for its codec id), add the
# object file. Prints "changed" if the tree needs configure again.
add_ports() { # source tree
    local tree=$1 c
    for c in $ALL_CODECS; do
        load_codec "$c"
        cp "$PROJ/shim/${c}_rs.c" "$tree/libavcodec/${c}_rs.c"
        if ! grep -q "ff_${c}_rs_decoder" "$tree/libavcodec/allcodecs.c"; then
            sed -i "s/^extern const FFCodec ff_${C_DECODER}_decoder;/extern const FFCodec ff_${c}_rs_decoder;\n&/" "$tree/libavcodec/allcodecs.c"
            sed -i "0,/^OBJS-\$(CONFIG_${CONFIG}_DECODER) .*/s//&\nOBJS-\$(CONFIG_${CONFIG}_RS_DECODER) += ${c}_rs.o/" "$tree/libavcodec/Makefile"
            grep -q "ff_${c}_rs_decoder" "$tree/libavcodec/allcodecs.c" && grep -q "${c}_rs.o" "$tree/libavcodec/Makefile" ||
                { echo "could not register ${c}_rs" >&2; exit 1; }
            echo changed
        fi
    done
}
