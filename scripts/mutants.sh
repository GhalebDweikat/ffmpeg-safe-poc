#!/bin/bash
# Runs inside the container. Checks that the checks have teeth: builds the
# deliberately broken variants of one codec's Rust decoder (listed in its
# description file) and records which test layer notices each one. The good
# library is restored afterwards.
#   mutants.sh CODEC
set -uo pipefail
. /proj/scripts/common.sh
load_codec "${1:?codec}"

WORK=/build/mutants
LIBS="$SRC/libavformat/libavformat.a $SRC/libavcodec/libavcodec.a $SRC/libswresample/libswresample.a $SRC/libavutil/libavutil.a"

rm -rf "$WORK"
mkdir -p "$WORK"
cp "$RUSTLIB" "$WORK/good.a"
restore() {
    cp "$WORK/good.a" "$RUSTLIB"
    (cd "$SRC" && rm -f ffmpeg ffmpeg_g && make -j"$(nproc)" >/dev/null 2>&1)
}
trap restore EXIT

fate() { (cd "$SRC" && make "$1" SAMPLES="$SAMPLES" >/dev/null 2>&1); }
fate_group() { # test names -> "caught" if any fails, "-" if the group is empty
    local t r=missed
    [ -n "$1" ] || { echo "-"; return; }
    for t in $1; do fate "$t" || r=caught; done
    echo "$r"
}

printf "%-36s %-11s %-17s %-19s %s\n" "mutant" "unit tests" "FATE real sample" "FATE encoder tests" "differential fuzz (10 s)"

for m in "${MUTANTS[@]}"; do
    name=${m%%|*}
    expr=${m#*|}
    rm -rf "$WORK/src"
    cp -r "$PROJ/rust" "$WORK/src"
    file="$WORK/src/$CODEC-core/src/lib.rs"
    before=$(md5sum <"$file")
    sed -i "$expr" "$file"
    if [ "$before" = "$(md5sum <"$file")" ]; then
        echo "$name: mutation did not apply"
        continue
    fi
    if ! (cd "$WORK/src" && CARGO_TARGET_DIR="$WORK/target" cargo build --release -p ffsafe-ffi >"$WORK/build.log" 2>&1); then
        echo "$name: mutant does not compile"
        continue
    fi

    # (Not a pipeline: with pipefail, a failing "cargo test" would hide grep's answer.)
    (cd "$WORK/src" && CARGO_TARGET_DIR="$WORK/target" cargo test --release -p "$CODEC-core" >"$WORK/unit.log" 2>&1)
    grep -q "test result: FAILED" "$WORK/unit.log" && unit=caught || unit=missed

    cp "$WORK/target/release/libffsafe_ffi.a" "$RUSTLIB"
    (cd "$SRC" && rm -f ffmpeg ffmpeg_g && make -j"$(nproc)" >/dev/null 2>&1)
    real=$(fate_group "$FATE_REAL")
    enc=$(fate_group "$FATE_ENC")

    gcc -O2 -I"$SRC" "$PROJ/harness/difffuzz.c" "$PROJ/harness/common.c" "$PROJ"/harness/codec_*.c \
        $LIBS "$RUSTLIB" $SYSLIBS -o "$WORK/difffuzz"
    out=$(cd "$WORK" && ./difffuzz "$CODEC" 7 10 | head -n 1)
    case "$out" in
    RESULT*) fuzz=missed ;;
    *) fuzz="caught: $(echo "$out" | grep -oE "^[A-Z_]+|case=[0-9]+" | tr '\n' ' ')" ;;
    esac

    printf "%-36s %-11s %-17s %-19s %s\n" "$name" "$unit" "$real" "$enc" "$fuzz"
done

restore
trap - EXIT
echo "-- good library restored; FATE again:"
for t in $FATE_REAL $FATE_ENC; do
    fate "$t" && echo "PASS $t" || echo "FAIL $t"
done
