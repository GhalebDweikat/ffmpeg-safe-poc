#!/bin/bash
# Runs inside the container, in the main tree. Every check for one decoder,
# with the raw output saved under results/ and a short summary printed.
#   verify.sh NAME [FUZZ_SECS] [ASAN_SECS] [STEPS]
# STEPS is any of: fate fuzz asan mutants bench (default: all but bench,
# which wants an otherwise idle machine).
set -uo pipefail
. /proj/scripts/common.sh
c=${1:?codec}
FUZZ_SECS=${2:-180}
ASAN_SECS=${3:-90}
STEPS=${4:-fate fuzz asan mutants}
R=$PROJ/results
S=$PROJ/scripts

for step in $STEPS; do
    case $step in
    fate)
        bash "$S/fate.sh" "$c" >"$R/$c-fate.txt" 2>&1
        grep -E "^FATE|^FAIL|Stream #" "$R/$c-fate.txt"
        ;;
    fuzz)
        bash "$S/fuzz.sh" "$c" "$FUZZ_SECS" 10 >"$R/$c-fuzz.txt" 2>&1
        echo "fuzz: $(grep -E "^no mismatches|FINDINGS|^MISMATCH|^DIRECT" "$R/$c-fuzz.txt" | head -n 3 | tr '\n' ' ') $(grep -E "^(clean runs|packets)" "$R/$c-fuzz.txt" | tr -s ' ' | tr '\n' ' ')"
        ;;
    asan)
        bash "$S/asan.sh" "$c" "$ASAN_SECS" 6 >"$R/$c-fuzz-asan.txt" 2>&1
        echo "asan: $(grep -E "^no mismatches|FINDINGS|^MISMATCH|ERROR: |runtime error|SANITIZER" "$R/$c-fuzz-asan.txt" | head -n 3 | tr '\n' ' ') $(grep -E "^(clean runs|packets)" "$R/$c-fuzz-asan.txt" | tr -s ' ' | tr '\n' ' ')"
        ;;
    mutants)
        bash "$S/mutants.sh" "$c" >"$R/$c-mutants.txt" 2>&1
        cat "$R/$c-mutants.txt"
        ;;
    bench)
        bash "$S/bench.sh" "$c" 31 >"$R/$c-bench-final.txt" 2>&1
        cat "$R/$c-bench-final.txt"
        ;;
    esac
done
