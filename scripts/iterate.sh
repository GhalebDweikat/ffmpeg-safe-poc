#!/bin/bash
# Runs inside the container. One turn of the optimisation loop for one
# codec: rebuild the Rust decoder, run its unit tests, relink the harnesses,
# run a short differential fuzz as a correctness gate, then benchmark.
#   iterate.sh CODEC [BENCH_PASSES] [FUZZ_SECONDS] [clips|paths|all]
# BENCH_PASSES 0 skips the benchmark.
set -uo pipefail
. /proj/scripts/common.sh
load_codec "${1:?codec}"

PASSES=${2:-21}
FUZZ=${3:-8}

cd "$PROJ/rust"
cargo test --release -p "$CODEC-core" 2>&1 | grep -E "^test .*FAILED|test result: .* [1-9][0-9]* passed|panicked|^error|^warning"
cargo build --release -p ffsafe-ffi 2>&1 | grep -E "^error|^warning|Finished"
bash "$PROJ/scripts/harness.sh" build-only || { echo "harness build failed"; exit 1; }

echo "== fuzz gate (${FUZZ}s per process)"
cd /tmp
for s in 1 2 3 4 5 6; do "$HARNESS/difffuzz" "$CODEC" "$RANDOM$s" "$FUZZ" & done
for f in $FUZZ_SEEDS; do "$HARNESS/difffuzz" "$CODEC" "$RANDOM" "$FUZZ" "$CLIPS/$CODEC/$f" & done
wait
[ "$PASSES" = 0 ] && exit 0
echo "== bench"
bash "$PROJ/scripts/bench.sh" "$CODEC" "$PASSES" 6 "${4:-all}"
