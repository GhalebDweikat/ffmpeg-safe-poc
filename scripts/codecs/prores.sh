# Apple ProRes. FFmpeg's decoder does the 10-bit inverse DCT with
# hand-written SSE2/AVX assembly; the 12-bit one is C.
C_DECODER=prores
CONFIG=PRORES
FATE_REAL="fate-prores-422 fate-prores-422_hq fate-prores-422_lt fate-prores-422_proxy fate-prores-alpha fate-prores-alpha_skip fate-prores-transparency fate-prores-transparency_skip fate-prores-gray fate-prores-probe fate-mxf-probe-applehdr10"
FATE_ENC="fate-vsynth1-prores fate-vsynth2-prores fate-vsynth3-prores fate-vsynth_lena-prores fate-vsynth1-prores_int fate-vsynth2-prores_int fate-vsynth3-prores_int fate-vsynth_lena-prores_int fate-vsynth1-prores_444 fate-vsynth2-prores_444 fate-vsynth3-prores_444 fate-vsynth_lena-prores_444 fate-vsynth1-prores_444_int fate-vsynth2-prores_444_int fate-vsynth3-prores_444_int fate-vsynth_lena-prores_444_int fate-vsynth1-prores_ks fate-vsynth2-prores_ks fate-vsynth3-prores_ks fate-vsynth_lena-prores_ks"
FATE_SAMPLES="prores/Sequence_1-Apple_ProRes_422.mov prores/Sequence_1-Apple_ProRes_422_HQ.mov prores/Sequence_1-Apple_ProRes_422_LT.mov prores/Sequence_1-Apple_ProRes_422_Proxy.mov prores/Sequence_1-Apple_ProRes_with_Alpha.mov prores/prores4444_with_transparency.mov prores/gray.mov mxf/Meridian-Apple_ProResProxy-HDR10.mxf lena.pnm"
FUZZ_SEEDS="seed-mix10.mov seed-mix12.mov seed-real.mov"
SYNTH_SIZE=1280x720
SYNTH_FRAMES=12

# Several small encodes joined into one stream, so that one fuzz process
# mutates packets of every kind (the decoder takes the picture size and
# layout from each frame's header; only the bit depth comes from the
# stream's tag).
join() { # output name, tag, part names...
    local out=$1 tag=$2 list="$CLIPS/$CODEC/$1.txt" p
    shift 2
    [ -s "$CLIPS/$CODEC/$out" ] && return
    : >"$list"
    for p in "$@"; do echo "file '$CLIPS/$CODEC/$p'" >>"$list"; done
    enc "$out" -f concat -safe 0 -i "$list" -c copy -tag:v "$tag"
}

make_clips() {
    local ts="testsrc2=rate=30:size" a="format=yuva444p10le"
    # Benchmark clips. 10-bit decoding (4:2:2 profiles): the assembly IDCT.
    enc hq-testsrc2-720p.mov     -f lavfi -i "$ts=1280x720" -frames:v 24 -pix_fmt yuv422p10le -c:v prores_ks -profile:v hq
    enc proxy-testsrc2-720p.mov  -f lavfi -i "$ts=1280x720" -frames:v 24 -pix_fmt yuv422p10le -c:v prores_ks -profile:v proxy
    enc hq-mandelbrot-720p.mov   -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 24 -pix_fmt yuv422p10le -c:v prores_ks -profile:v hq
    enc hq-photo-704x576.mov     -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -frames:v 30 -pix_fmt yuv422p10le -c:v prores_ks -profile:v hq
    enc aw-testsrc2-1080i.mov    -f lavfi -i "$ts=1920x1080" -frames:v 12 -pix_fmt yuv422p10le -c:v prores -flags +ildct
    enc ks-testsrc2-1080i.mov    -f lavfi -i "$ts=1920x1080" -frames:v 12 -pix_fmt yuv422p10le -c:v prores_ks -profile:v standard -flags +ildct
    # 12-bit decoding (4444 profiles): the C IDCT. With and without alpha.
    enc 4444-testsrc2-720p.mov   -f lavfi -i "$ts=1280x720" -frames:v 16 -pix_fmt yuv444p10le -c:v prores_ks -profile:v 4444
    enc 4444a-testsrc2-720p.mov  -f lavfi -i "$ts=1280x720:alpha=200,$a" -f lavfi -i "mandelbrot=size=1280x720:rate=30,format=gray" -filter_complex "[0][1]alphamerge,$a" -frames:v 16 -c:v prores_ks -profile:v 4444 -alpha_bits 16
    enc xq-mandelbrot-720p.mov   -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 16 -pix_fmt yuv444p10le -c:v prores_ks -profile:v 4444xq
    real prores/Sequence_1-Apple_ProRes_422_HQ.mov     real-422-hq.mov
    real prores/Sequence_1-Apple_ProRes_422_Proxy.mov  real-422-proxy.mov
    real prores/Sequence_1-Apple_ProRes_with_Alpha.mov real-4444-alpha.mov
    real prores/prores4444_with_transparency.mov       real-4444-transparency.mov

    # Parts of the fuzz seeds: small, odd sizes, every layout.
    enc seed-p1.mov -f lavfi -i "$ts=160x120" -frames:v 6 -pix_fmt yuv422p10le -c:v prores_ks -profile:v hq
    enc seed-p2.mov -f lavfi -i "$ts=98x62"   -frames:v 6 -pix_fmt yuv422p10le -c:v prores_ks -profile:v proxy -flags +ildct
    enc seed-p3.mov -f lavfi -i "mandelbrot=size=74x50:rate=30" -frames:v 6 -pix_fmt yuv422p10le -c:v prores -flags +ildct -top 0
    enc seed-p4.mov -f lavfi -i "$ts=64x48"   -frames:v 6 -pix_fmt yuv444p10le -c:v prores_ks -profile:v 4444
    enc seed-p5.mov -f lavfi -i "$ts=98x62:alpha=200,$a" -f lavfi -i "mandelbrot=size=98x62:rate=30,format=gray" -filter_complex "[0][1]alphamerge,$a" -frames:v 6 -c:v prores_ks -profile:v 4444 -alpha_bits 16
    enc seed-p6.mov -f lavfi -i "$ts=98x62:alpha=200,$a" -f lavfi -i "mandelbrot=size=98x62:rate=30,format=gray" -filter_complex "[0][1]alphamerge,$a" -frames:v 6 -c:v prores_ks -profile:v 4444 -alpha_bits 8
    enc seed-p7.mov -f lavfi -i "$ts=126x34:alpha=90,$a" -frames:v 6 -c:v prores -flags +ildct
    enc seed-p8.mov -f lavfi -i "mandelbrot=size=33x17:rate=30" -frames:v 6 -pix_fmt yuv444p10le -c:v prores_ks -profile:v 4444xq -flags +ildct
    join seed-mix10.mov apch seed-p1.mov seed-p2.mov seed-p3.mov seed-p4.mov seed-p5.mov seed-p7.mov
    join seed-mix12.mov ap4h seed-p4.mov seed-p5.mov seed-p6.mov seed-p7.mov seed-p8.mov seed-p1.mov seed-p3.mov
    real prores/prores4444_with_transparency.mov seed-real.mov
}

# Deliberately broken variants: "name|sed expression for prores-core/src/*.rs".
MUTANTS=(
    "AVX2 IDCT: one constant wrong|s/m(p13, w\[3\], -w\[7\]), m(p57, w\[1\], w\[5\])/m(p13, w[3], -w[7]), m(p57, w[1], w[7])/"
    "portable IDCT: row rounding in 32 bits|s/x\[0\] = i32::from(row\[0\].wrapping_add(1));/x[0] = i32::from(row[0]) + 1;/"
    "12-bit DC-only row shortcut removed|s/let pick = |v| _mm256_blendv_epi8(v, dc, dc_only);/let pick = |v| v;/"
    "end-of-data test on a refilled window|s/let bits_left = r.bits_left();/r.update_cache(); let bits_left = r.bits_left();/"
    "second field on the first field's rows|s/first_field != self.opts.top_field_first/!self.opts.top_field_first/"
    "scaled quantiser saturates, not wraps|s/(i32::from(q) \* qscale) as i16/(i32::from(q) * qscale).min(32767) as i16/"
)
