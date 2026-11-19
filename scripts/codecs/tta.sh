# TTA (True Audio): lossless audio, adaptive Rice codes and an adaptive
# 8-tap filter for which FFmpeg has SSSE3/SSE4 assembly.
C_DECODER=tta
CONFIG=TTA
FATE_REAL="fate-lossless-tta fate-lossless-tta-encrypted fate-filter-metadata-silencedetect fate-ffmpeg-error-rate-fail fate-ffmpeg-error-rate-pass"
FATE_ENC="fate-acodec-tta fate-lavf-tta fate-lavf-mka"
FATE_SAMPLES="lossless-audio/inside.tta lossless-audio/encrypted.tta mkv/h264_tta_undecodable.mkv"
# Seed clips for the mutation fuzzer. Two by default, to stay within the
# CPU share of a parallel porting session; TTA_FUZZ_SEEDS selects others
# (seed-undecodable.mkv seed-sine-8k-mono.tta seed-noise-s24-11k.tta
# seed-u8-6ch.mka) for further rounds.
FUZZ_SEEDS="${TTA_FUZZ_SEEDS:-real-inside.tta seed-encrypted.tta}"
DEMUXERS="tta,matroska"
# The benchmark's single-path streams: "size" is sample rate x (unused),
# every packet is one frame of 256 * rate / 245 samples per channel.
SYNTH_SIZE=44100x1
SYNTH_FRAMES=40

make_clips() {
    local noise="anoisesrc=d=20:c=pink:r=44100:a=0.5:seed=7"
    # A mix of tones and noise, as a stand-in for music.
    local mix="sine=f=220:r=44100:d=30[a];sine=f=1397:r=44100:d=30[b];anoisesrc=d=30:c=brown:r=44100:a=0.3:seed=3[c];[a][b][c]amix=inputs=3:normalize=0"
    enc sine-s16-stereo.tta   -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=30" -ac 2 -c:a tta
    enc mix-s16-stereo.tta    -f lavfi -i "$mix" -ac 2 -sample_fmt s16 -c:a tta
    enc noise-s16-stereo.tta  -f lavfi -i "$noise" -ac 2 -sample_fmt s16 -c:a tta
    enc mix-s24-stereo.tta    -f lavfi -i "$mix" -ac 2 -sample_fmt s32 -c:a tta
    enc noise-s24-mono.tta    -f lavfi -i "$noise" -ac 1 -sample_fmt s32 -c:a tta
    enc mix-u8-stereo.tta     -f lavfi -i "$mix" -ac 2 -sample_fmt u8 -c:a tta
    enc mix-s16-mono-48k.tta  -f lavfi -i "$mix" -ac 1 -ar 48000 -sample_fmt s16 -c:a tta
    enc mix-s16-6ch-48k.mka   -f lavfi -i "$mix" -ac 6 -ar 48000 -sample_fmt s16 -c:a tta
    enc mix-s24-8ch-96k.mka   -f lavfi -i "$mix" -t 8 -ac 8 -ar 96000 -sample_fmt s32 -c:a tta
    enc seed-sine-8k-mono.tta  -f lavfi -i "sine=frequency=300:sample_rate=8000:duration=6" -ac 1 -c:a tta
    enc seed-noise-s24-11k.tta -f lavfi -i "$noise" -t 4 -ac 2 -ar 11025 -sample_fmt s32 -c:a tta
    enc seed-u8-6ch.mka        -f lavfi -i "$mix" -t 4 -ac 6 -ar 8000 -sample_fmt u8 -c:a tta
    real lossless-audio/inside.tta      real-inside.tta
    real lossless-audio/encrypted.tta   seed-encrypted.tta
    real mkv/h264_tta_undecodable.mkv   seed-undecodable.mkv
}

# Deliberately broken variants: "name|sed expression for tta-core/src/*.rs".
MUTANTS=(
    "SSE filter: one sign-step constant wrong|s/_mm_setr_epi32(1, 2, 2, 4)/_mm_setr_epi32(1, 2, 2, 2)/"
    "portable filter: wrong mask for dx[7]|s/((dl\[7\] >> 30) | 4) \& !3,/((dl[7] >> 30) | 4) \& !1,/"
    "value overflow check dropped|s/if unary > (i32::MAX as u32) >> k {/if false {/"
    "second Rice parameter adapted when unused|s/rice.k\[1\] = rice.k\[1\].wrapping_add(adapt(rice.k\[1\], sum1) \& mask);/rice.k[1] = rice.k[1].wrapping_add(adapt(rice.k[1], sum1));/"
    "8-bit streams use the 16-bit predictor|s/let pred_shift = if h.bps == 1 { 4 } else { 5 };/let pred_shift = 5;/"
    "31 bits accepted in place of the CRC|s/if bits.left() < 32 {/if bits.left() < 31 {/"
    "end of packet: bit window may fill to 64 bits|s/while self.count < 56 {/while self.count <= 56 {/"
)
