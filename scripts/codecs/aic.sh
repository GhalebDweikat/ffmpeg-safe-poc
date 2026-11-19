# Apple Intermediate Codec. FFmpeg has no encoder for it, so the two real
# samples of FFmpeg's test suite are the only clips; everything else comes
# from the packet generator in harness/codec_aic.c.
C_DECODER=aic
CONFIG=AIC
FATE_REAL="fate-aic fate-aic-oddsize"
FATE_ENC=""
FATE_SAMPLES="aic/small_apple_intermediate_codec.mov aic/aic_odd_dimensions.mov"
FUZZ_SEEDS="real-small.mov real-oddsize.mov"

make_clips() {
    real aic/small_apple_intermediate_codec.mov real-small.mov
    real aic/aic_odd_dimensions.mov             real-oddsize.mov
}

# Deliberately broken variants: "name|sed expression for aic-core/src/lib.rs".
MUTANTS=(
    "x86 IDCT: rows cut, not saturated|s/_mm_set1_epi32(1 << (ROW_SHIFT - 1)), !x86)/_mm_set1_epi32(1 << (ROW_SHIFT - 1)), true)/"
    "x86 IDCT: rounding added in 32 bits|s/let round = if x86 {/let round = if false {/"
    "row with one coefficient transformed|s/let dc_only = _mm_cmpeq_epi16(rest, _mm_setzero_si128());/let dc_only = _mm_setzero_si128();/"
    "buffer cleared for every slice|s/if slice_width == self.slice_width {/if true {/"
    "reader goes on past the slice end|s/self.index = self.size_in_bits_plus8.min(self.index + n);/self.index += n;/"
    "longest Exp-Golomb code rejected|s/if zeros > 12 {/if zeros > 11 {/"
    "last row not moved up to the edge|s/let last_row = mb_y != 0 \&\& /let last_row = mb_y == 0 \&\& /"
    "luma not moved on interlace change|s/if self.interlaced_layout != hdr.interlaced {/if false {/"
    "one quantisation matrix entry wrong|s/^     8, 16, 19, 22, 22, 26, 26, 27,/     8, 16, 19, 22, 22, 26, 26, 28,/"
)
