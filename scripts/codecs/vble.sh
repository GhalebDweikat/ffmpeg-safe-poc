# VBLE Lossless Codec: 8-bit 4:2:0, unary code lengths plus raw residual
# bits, left and median prediction. FFmpeg's decoder runs the median
# predictor through hand-written SSE2 assembly.
C_DECODER=vble
CONFIG=VBLE
FATE_REAL="fate-vble"
FATE_SAMPLES="vble/flowers-partial-2MB.avi"
FUZZ_SEEDS="real-flowers.avi"

make_clips() {
    # FFmpeg has no VBLE encoder, so the only clip is the real sample; the
    # benchmark's other rows are streams made by the encoder in
    # harness/codec_vble.c.
    real vble/flowers-partial-2MB.avi real-flowers.avi
}

# Deliberately broken variants: "name|sed expression for vble-core/src/lib.rs".
MUTANTS=(
    "length table: gap starts a bit early|s/start = bit + 1;/start = bit;/"
    "bits-left check off by one|s/is_none_or(|left| left < allbits)/is_none_or(|left| left <= allbits)/"
    "residual sign ignored|s/((x >> 1) ^ (x \& 1).wrapping_neg()) as u8/(x >> 1) as u8/"
    "row start predicted from above|s/row_cells\[0\]\[k \& 15\] = first.wrapping_sub(first_above);/row_cells[0][k \& 15] = first;/"
    "SSE2 overwrite after odd rows dropped|s/let overwrite = opts.sse2_overwrite \&\&/let overwrite = false \&\&/"
    "zero-width plane gets no sample|s/let rows = if overwrite { height } else { height.min(1) };/let rows = if overwrite { height } else { 0 };/"
    "SSE2 predictor: clamp on wrong side|s/_mm_max_epu8(low, _mm_min_epu8(high, gradient))/_mm_max_epu8(low, _mm_max_epu8(high, gradient))/"
    "portable predictor: no top-left|s/let gradient = l.wrapping_add(t).wrapping_sub(top_left\[k\]);/let gradient = l.wrapping_add(t);/"
)
