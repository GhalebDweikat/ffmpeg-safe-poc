#!/bin/bash
# Runs inside the container. Long differential fuzz of one codec's Rust port
# against the C decoder: GEN processes on generated streams plus one process
# per seed clip mutating real packets, SECS seconds each.
#   fuzz.sh CODEC [SECS] [GEN] [HARNESS_BINARY]
set -uo pipefail
. /proj/scripts/common.sh
load_codec "${1:?codec}"

SECS=${2:-180}
GEN=${3:-16}
BIN=${4:-$HARNESS/difffuzz}
LOGS=$(mktemp -d)
cd "$LOGS"

# One fuzz process. Output is capped at 1 MB, and the process is killed if it
# overruns its time. gcc 12's AddressSanitizer runtime sometimes crashes in a
# loop before main() under address-space randomisation ("DEADLYSIGNAL" with
# no other output); that is a toolchain fault, so such a start is retried.
run_one() { # log, harness arguments...
    local log=$1 attempt
    shift
    for attempt in 1 2 3 4 5 6; do
        timeout -s KILL $((SECS + 120)) "$BIN" "$CODEC" "$@" 2>&1 | head -c 1000000 >"$log"
        if grep -q "^RESULT\|MISMATCH" "$log" || ! grep -q "DEADLYSIGNAL" "$log"; then
            return
        fi
    done
    echo "SANITIZER_NEVER_STARTED $*" >"$log"
}

for i in $(seq 1 "$GEN"); do
    run_one "gen-$i.log" $((1000 + i)) "$SECS" &
done
k=0
for clip in $FUZZ_SEEDS; do
    k=$((k + 1))
    run_one "real-$clip.log" $((2000 + k)) "$SECS" "$CLIPS/$CODEC/$clip" &
done
wait

echo "codec: $CODEC; processes: $(ls | wc -l), ${SECS}s each"
# Anything that is not a clean RESULT line is a finding.
if grep -hv "^RESULT .*mismatches=0 panics=0$" ./*.log | head -n 60 | grep .; then
    echo "FINDINGS ABOVE"
else
    echo "no mismatches, no panics, no sanitizer reports"
fi
cat ./*.log | awk '
    /^RESULT/ {
        for (i = 2; i <= NF; i++) {
            split($i, kv, "=")
            if (kv[2] ~ /^[0-9]+$/ && kv[1] != "seed") {
                if (!(kv[1] in sum)) order[++n] = kv[1]
                sum[kv[1]] += kv[2]
            }
        }
        runs++
    }
    END {
        printf "clean runs: %d\n", runs
        for (i = 1; i <= n; i++) printf "%-16s %d\n", order[i], sum[order[i]]
    }'
rm -rf "$LOGS"
