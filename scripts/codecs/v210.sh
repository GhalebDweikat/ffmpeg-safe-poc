# v210: uncompressed 10-bit 4:2:2. FFmpeg's decoder uses hand-written
# SSSE3/AVX/AVX2/AVX-512 assembly for the unpacking.
C_DECODER=v210
CONFIG=V210
FATE_REAL="fate-v210"
FATE_ENC="fate-vsynth1-v210 fate-vsynth2-v210 fate-vsynth3-v210 fate-vsynth_lena-v210 fate-vsynth1-v210-10 fate-vsynth2-v210-10 fate-vsynth3-v210-10 fate-vsynth_lena-v210-10"
FATE_SAMPLES="v210/v210_720p-partial.avi lena.pnm"
FUZZ_SEEDS="real-v210-720p.avi seed-testsrc-160x120.avi seed-mandel-98x62.avi"

make_clips() {
    enc testsrc2-720p.avi   -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 60 -c:v v210
    enc testsrc2-1080p.avi  -f lavfi -i "testsrc2=size=1920x1080:rate=30" -frames:v 30 -c:v v210
    enc mandelbrot-720p.avi -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 60 -c:v v210
    # A width that is not a multiple of 6, so every row has a tail (the
    # encoder only takes even widths).
    enc tail-638x354.avi    -f lavfi -i "testsrc2=size=638x354:rate=30" -frames:v 100 -c:v v210
    enc seed-testsrc-160x120.avi -f lavfi -i "testsrc2=size=160x120:rate=30" -frames:v 20 -c:v v210
    enc seed-mandel-98x62.avi    -f lavfi -i "mandelbrot=size=98x62:rate=30" -frames:v 20 -c:v v210
    real v210/v210_720p-partial.avi real-v210-720p.avi
}

# Deliberately broken variants: "name|sed expression for v210-core/src/lib.rs".
MUTANTS=(
    "AVX2: one luma multiplier wrong|s/let luma_mul = _mm256_setr_epi16(16, 64, 4, 16, 64, 4, 0, 0, 16,/let luma_mul = _mm256_setr_epi16(16, 64, 4, 16, 64, 4, 0, 0, 32,/"
    "portable: third luma sample from wrong field|s/\*y = \[s1(w0), s0(w1), s2(w1), s1(w2), s0(w3), s2(w3)\];/*y = [s1(w0), s0(w1), s1(w1), s1(w2), s0(w3), s2(w3)];/"
    "row tail: third chroma sample wrong|s/let us = \[s0(w0), s1(w1), s2(w2)\];/let us = [s0(w0), s1(w1), s1(w2)];/"
    "short-padding guess misses 12-pixel rows|s/let found = \[24, 12, 6\]/let found = [24, 6]/"
    "INFO header not skipped|s/offset = 64;/offset = 0;/"
    "unpadded rows: one chroma word off|s/1 => (s1(word(src, g + 1)), s0(word(src, g + 2))),/1 => (s1(word(src, g + 1)), s0(word(src, g + 3))),/"
)
