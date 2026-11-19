# LOCO lossless video (AVI). FFmpeg has no LOCO encoder, so the only clips
# are the two real samples of its test suite; everything else is generated
# by the Golomb-Rice writer in harness/codec_loco.c.
C_DECODER=loco
CONFIG=LOCO
FATE_REAL="fate-loco-rgb fate-loco-yuy2"
FATE_SAMPLES="loco/pig-loco-rgb.avi loco/pig-loco-0.avi"
FUZZ_SEEDS="real-pig-rgb.avi real-pig-yuy2.avi"
# A frame takes milliseconds to decode, so the single-path streams are short.
SYNTH_FRAMES=12

make_clips() {
    real loco/pig-loco-rgb.avi real-pig-rgb.avi
    real loco/pig-loco-0.avi   real-pig-yuy2.avi
}

# Deliberately broken variants: "name|sed expression for loco-core/src/*.rs".
MUTANTS=(
    "read past the packet end accepted|s/self.n.checked_sub(self.past_end)? as usize/self.n.saturating_sub(self.past_end) as usize/"
    "Rice parameter capped at 8, not 9|s/while sum > val \&\& cnt < 9 {/while sum > val \&\& cnt < 8 {/"
    "zero run of 1 counted as a long run|s/save = if run > 1 { save.wrapping_add(run + 1) }/save = if run > 0 { save.wrapping_add(run + 1) }/"
    "odd-width RGB not sheared back|s/if format == Format::Gbr \&\& width \& 1 != 0 {/if false {/"
    "lossy offset not added|s/residual = ((v >> 1).wrapping_add(lossy) ^ 0u32.wrapping_sub(v \& 1)) as u8;/residual = ((v >> 1) ^ 0u32.wrapping_sub(v \& 1)) as u8;/"
    "code value of all ones not an error|s/(self, if v == u32::MAX { None } else { Some(v) })/(self, Some(v))/"
)
