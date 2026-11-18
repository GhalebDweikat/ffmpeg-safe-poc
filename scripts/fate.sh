#!/bin/bash
# Runs inside the container. Fetches the FATE sample files a codec's tests
# need and runs FFmpeg's own tests for it, unmodified. Because each port is
# registered ahead of the C decoder, these tests exercise the Rust code.
#   fate.sh [CODEC ...]     (default: every ported codec)
set -uo pipefail
. /proj/scripts/common.sh

cd "$SRC"
total_fail=0
for c in ${@:-$ALL_CODECS}; do
    load_codec "$c"
    for f in $FATE_SAMPLES; do
        mkdir -p "$SAMPLES/$(dirname "$f")"
        [ -s "$SAMPLES/$f" ] || curl -fsSL "https://fate-suite.ffmpeg.org/$f" -o "$SAMPLES/$f"
    done
    first=$SAMPLES/${FATE_SAMPLES%% *}
    echo "== $c: decoder FFmpeg picks by default"
    ./ffmpeg -hide_banner -i "$first" -an -frames:v 1 -f null - 2>&1 | grep -E "Stream #0:[0-9]+ ->" | head -n 1
    pass=0; fail=0
    for t in $FATE_REAL $FATE_ENC; do
        if make "$t" SAMPLES="$SAMPLES" >"/tmp/$t.log" 2>&1; then
            echo "PASS $t"; pass=$((pass + 1))
        else
            echo "FAIL $t"; tail -n 12 "/tmp/$t.log"; fail=$((fail + 1))
        fi
    done
    echo "FATE $c: $pass passed, $fail failed"
    total_fail=$((total_fail + fail))
done
exit $((total_fail > 0))
