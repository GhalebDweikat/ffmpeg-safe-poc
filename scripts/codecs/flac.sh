# FLAC (Free Lossless Audio Codec).
C_DECODER=flac
CONFIG=FLAC
# Tests that decode existing FLAC files (plain .flac, Matroska, Ogg).
FATE_REAL="fate-filter-hdcd-mix fate-filter-hdcd-mix-psoff fate-filter-hdcd-analyze-pe fate-filter-hdcd-analyze-lle fate-filter-hdcd-analyze-cdt fate-filter-hdcd-analyze-tgm fate-filter-hdcd-analyze-ltgm fate-filter-hdcd-analyze-pel fate-filter-hdcd-false-positive fate-filter-hdcd-detect-errors fate-filter-hdcd-20bit fate-filter-metadata-ebur128 fate-filter-metadata-avf-aphase-meter-out-of-phase fate-pcm_dvd-16-5.1-48000 fate-pcm_dvd-24-2-48000 fate-matroska-flac-extradata-update fate-ogg-flac-chained-meta fate-ogg-flac-copy-chained-meta fate-matroska-flac-channel-mapping fate-cover-art-flac"
# Tests that encode with FFmpeg's FLAC encoder and decode the result.
FATE_ENC="fate-flac-16-chmode-indep fate-flac-16-chmode-left_side fate-flac-16-chmode-mid_side fate-flac-16-chmode-right_side fate-flac-16-fixed fate-flac-16-lpc-cholesky fate-flac-16-lpc-levinson fate-flac-24-comp-8 fate-flac-32-wasted-bits fate-flac-rice-params fate-acodec-flac fate-acodec-flac-exact-rice fate-api-flac fate-lavf-ogg fate-seek-acodec-flac fate-seek-lavf-ogg"
FATE_SAMPLES="filter/hdcd-mix.flac filter/hdcd-false-positive.flac filter/hdcd-encoding-errors.flac filter/hdcd-fake20bit.flac filter/seq-3341-7_seq-3342-5-24bit.flac filter/out-of-phase-1000hz.flac mkv/flac_channel_layouts.mka ogg-flac/chained-meta.ogg cover_art/cover_art.flac audio-reference/luckynight_2ch_44kHz_s16.wav audio-reference/divertimenti_2ch_96kHz_s24.wav audio-reference/drums_2ch_44kHz_s32_wastedbits.wav audio-reference/chorusnoise_2ch_44kHz_s16.wav"
DEMUXERS="flac,ogg,matroska"
PARSERS="flac"
FUZZ_SEEDS="seed-music-s16.flac seed-music-s24.flac seed-s32-wasted.flac seed-51-s16.flac seed-mono-8k.flac seed-ogg.ogg real-hdcd-mix.flac real-layouts.mka real-chained.ogg"
# The benchmark's single-path streams: "WxH" is block size x channels.
SYNTH_SIZE=4096x2
SYNTH_FRAMES=200

make_clips() {
    local ref="$SAMPLES/audio-reference"
    local music="-stream_loop 3 -i $ref/luckynight_2ch_44kHz_s16.wav"
    local hires="-stream_loop 3 -i $ref/divertimenti_2ch_96kHz_s24.wav"
    # Real music, 16 bit 44.1 kHz stereo, at the encoder's fastest, default
    # and slowest settings, and with fixed predictors only.
    enc music-s16-level0.flac  $music -c:a flac -compression_level 0
    enc music-s16-level5.flac  $music -c:a flac -compression_level 5
    enc music-s16-level8.flac  $music -c:a flac -compression_level 8
    enc music-s16-level12.flac $music -c:a flac -compression_level 12
    # Coefficients of 10 bits, as libFLAC writes for 16-bit audio: these
    # take the 32-bit LPC sum, the others the 64-bit one.
    enc music-s16-prec10.flac  $music -c:a flac -compression_level 8 -lpc_coeff_precision 10
    enc music-s16-order32.flac $music -c:a flac -compression_level 12 -min_prediction_order 32 -max_prediction_order 32 -lpc_coeff_precision 10
    # Real music, 24 bit 96 kHz stereo.
    enc music-s24-level5.flac  $hires -c:a flac -compression_level 5
    enc music-s24-level8.flac  $hires -c:a flac -compression_level 8
    enc music-s24-order32.flac $hires -c:a flac -compression_level 12 -min_prediction_order 32 -max_prediction_order 32
    # 32 bit with unused low bits (wasted bits, and 33-bit side channels).
    enc drums-s32-wasted.flac  -stream_loop 3 -i "$ref/drums_2ch_44kHz_s32_wastedbits.wav" -c:a flac -strict experimental
    enc noise-s32.flac         -f lavfi -i "anoisesrc=d=20:c=pink:r=48000:a=0.4,aformat=sample_fmts=s32:channel_layouts=stereo" -c:a flac -strict experimental -bits_per_raw_sample 32
    # Noisy choir (large Rice parameters), 5.1 and 7.1, mono, 8 kHz.
    enc chorus-s16-level5.flac -stream_loop 3 -i "$ref/chorusnoise_2ch_44kHz_s16.wav" -c:a flac -compression_level 5
    enc music-51-s16.flac      $music -af "pan=5.1|FL=FL|FR=FR|FC=0.5*FL+0.5*FR|LFE=0.1*FL|BL=0.7*FL|BR=0.7*FR" -c:a flac
    enc music-71-s24.flac      $hires -af "pan=7.1|FL=FL|FR=FR|FC=0.5*FL+0.5*FR|LFE=0.1*FL|BL=0.7*FL|BR=0.7*FR|SL=0.3*FL|SR=0.3*FR" -c:a flac
    enc music-mono-s16.flac    $music -ac 1 -c:a flac
    enc sine-s16.flac          -f lavfi -i "sine=frequency=440:duration=60" -c:a flac
    # Small clips used as fuzz seeds.
    enc seed-music-s16.flac    -t 1.5 -i "$ref/luckynight_2ch_44kHz_s16.wav" -c:a flac -compression_level 8 -frame_size 1152
    enc seed-music-s24.flac    -t 0.7 -i "$ref/divertimenti_2ch_96kHz_s24.wav" -c:a flac -compression_level 12 -frame_size 2304
    enc seed-s32-wasted.flac   -t 1 -i "$ref/drums_2ch_44kHz_s32_wastedbits.wav" -c:a flac -strict experimental -frame_size 1024
    enc seed-51-s16.flac       -t 1 -i "$ref/luckynight_2ch_44kHz_s16.wav" -af "pan=5.1|FL=FL|FR=FR|FC=0.5*FL+0.5*FR|LFE=0.1*FL|BL=0.7*FL|BR=0.7*FR" -c:a flac -frame_size 576
    enc seed-mono-8k.flac      -f lavfi -i "anoisesrc=d=2:c=brown:r=8000:a=0.5" -c:a flac -frame_size 192 -lpc_type fixed
    enc seed-ogg.ogg           -t 1 -i "$ref/chorusnoise_2ch_44kHz_s16.wav" -c:a flac -frame_size 512
    real filter/hdcd-mix.flac            real-hdcd-mix.flac
    real mkv/flac_channel_layouts.mka    real-layouts.mka
    real ogg-flac/chained-meta.ogg       real-chained.ogg
}

# Deliberately broken variants: "name|sed expression for the files of flac-core/src".
MUTANTS=(
    "frame number: bit 6 of a one-byte number read as a length|s#let mut bit = (first & 0x80) >> 1;#let mut bit = 0x40;#"
    "Rice code: the escape taken for an ordinary code|s#let fast_max = (55 - k).min(limit - 3);#let fast_max = (55 - k).min(limit - 1);#"
    "escaped partition read as unsigned|s#\*o = ((f.buf as i64) >> (64 - n)) as i32;#*o = (f.buf >> (64 - n)) as i32;#"
    "LPC: 32-bit sum used one bit too far|s#order.ilog2() as i32 <= 32#order.ilog2() as i32 <= 33#"
    "mid/side: side halved by division, not shift|s#let a = a.wrapping_sub(b >> 1);#let a = a.wrapping_sub(b / 2);#"
    "16-bit output truncated where the assembly saturates|s#let s = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX));#let s = v;#"
)

# Two of the tests are programs that link the Rust library themselves, and
# make does not know that they depend on it: drop them when the library is
# newer, so that they are linked again.
for _t in api-flac-test api-dump-stream-meta-test; do
    if [ -n "${SRC:-}" ] && [ -f "$SRC/tests/api/$_t" ] && [ "$RUSTLIB" -nt "$SRC/tests/api/$_t" ]; then
        rm -f "$SRC/tests/api/$_t"
    fi
done
