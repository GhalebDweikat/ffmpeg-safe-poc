#!/bin/bash
# Manual checks for the VBLE port (not part of the project's scripts).
# Run inside the container. Everything is written under /build/w/vble/tmp.
set -u
T=/build/w/vble/tmp
FF="/build/w/vble/ffmpeg/ffmpeg -hide_banner -loglevel error"
IN=/build/w/vble/clips/vble/real-flowers.avi
mkdir -p $T

crc() { # label, decoder, extra options..., input
    local label=$1 dec=$2
    shift 2
    printf "%-44s %s\n" "$label" "$($FF "${@:1:$#-1}" -c:v "$dec" -i "${@: -1}" -f framecrc - 2>/dev/null | grep '^0,' | md5sum | cut -c1-12)"
}

echo "== frame threads: same frames with 1 and 4 threads"
crc "C, 1 thread" vble -threads 1 $IN
crc "Rust, 1 thread" vble_rs -threads 1 $IN
crc "Rust, 4 frame threads" vble_rs -threads 4 -thread_type frame $IN

# The same file with its width field changed: the decoder then reads the
# packets as pictures of another size. The pictures are noise, but valid.
patch() { # new width, output
    python3 - "$1" "$2" <<EOF
import sys
d = bytearray(open("$IN", "rb").read())
old = (1280).to_bytes(4, "little") + (720).to_bytes(4, "little")
new = int(sys.argv[1]).to_bytes(4, "little") + (720).to_bytes(4, "little")
n = d.count(old)
open(sys.argv[2], "wb").write(d.replace(old, new))
print("patched", n, "width fields ->", sys.argv[1])
EOF
}
for w in 1279 1278 1249; do
    patch $w $T/w$w.avi
    echo "== width $w: C decoder with and without its assembly, and the port"
    crc "C, SSE2 predictor" vble $T/w$w.avi
    crc "C, -cpuflags 0 (C predictor)" vble -cpuflags 0 $T/w$w.avi
    crc "Rust" vble_rs $T/w$w.avi
    crc "Rust, -cpuflags 0" vble_rs -cpuflags 0 $T/w$w.avi
done
