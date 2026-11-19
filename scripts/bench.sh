#!/bin/bash
# Runs inside the container. Benchmarks FFmpeg's C decoder against the Rust
# port of one codec, pinned to one CPU. Prints one table row per input.
#   bench.sh CODEC [PASSES] [CPU] [clips|paths|all]
# "paths" times each decoder path on its own with single-path streams;
# "clips" times the encoded test clips.
set -uo pipefail
. /proj/scripts/common.sh
load_codec "${1:?codec}"

PASSES=${2:-31}
CPU=${3:-$BENCH_CPU}
WHAT=${4:-all}

row() { # label, bench argument
    local line
    line=$(taskset -c "$CPU" "$HARNESS/bench" "$CODEC" "$2" "$PASSES") || { echo "$1: $line"; return; }
    get() { echo "$line" | tr ' ' '\n' | grep "^$1=" | cut -d= -f2; }
    printf "%-22s %-9s %6s %12s %14s %8s %8s\n" "$1" "$(get size)" "$(get frames)" \
        "$(get c_us_per_frame)" "$(get rs_us_per_frame)" "$(get rs_over_c_median)" "$(get rs_over_c_min)"
}

printf "%-22s %-9s %6s %12s %14s %8s %8s\n" input size frames "C us/frame" "Rust us/frame" "Rust/C" "(min)"
if [ "$WHAT" != clips ]; then
    for kind in $("$HARNESS/bench" "$CODEC" kinds); do
        row "path:$kind" "synth:$kind:$SYNTH_SIZE:$SYNTH_FRAMES"
    done
fi
if [ "$WHAT" != paths ]; then
    for f in "$CLIPS/$CODEC"/*; do
        case "$f" in */seed-*) continue ;; esac
        row "$(basename "${f%.*}")" "$f"
    done
fi
