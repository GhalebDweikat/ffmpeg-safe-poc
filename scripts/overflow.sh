#!/bin/bash
# Runs inside the container. Release builds of Rust let integer overflow and
# over-wide shifts wrap silently; a port that relies on that by accident can
# still differ from C on rare input. This builds the Rust library with
# overflow checks and debug assertions on (so such an operation panics, which
# the fuzzer reports), runs one codec's unit tests in a debug build, and runs
# the differential fuzzer against the checked library.
#   overflow.sh CODEC [SECS] [GEN]
set -uo pipefail
. /proj/scripts/common.sh
load_codec "${1:?codec}"
SECS=${2:-120}
GEN=${3:-8}
T=$ROOT/rust-target-checked

cd "$PROJ/rust"
echo "== unit tests, debug build"
CARGO_TARGET_DIR=$T cargo test -p "$CODEC-core" 2>&1 | grep -E "^test .*FAILED|test result|panicked|^error"
echo "== library with overflow checks"
CARGO_TARGET_DIR=$T RUSTFLAGS="-C overflow-checks=on -C debug-assertions=on" \
    cargo build --release -p ffsafe-ffi $CARGO_FEATURES 2>&1 | grep -E "^error|Finished"
LIBS="$SRC/libavformat/libavformat.a $SRC/libavcodec/libavcodec.a $SRC/libswresample/libswresample.a $SRC/libavutil/libavutil.a $T/release/libffsafe_ffi.a $SYSLIBS"
gcc -O2 -I"$SRC" "$PROJ/harness/difffuzz.c" $(harness_sources) $LIBS -o "$HARNESS/difffuzz-checked" || exit 1
bash "$PROJ/scripts/fuzz.sh" "$CODEC" "$SECS" "$GEN" "$HARNESS/difffuzz-checked"
