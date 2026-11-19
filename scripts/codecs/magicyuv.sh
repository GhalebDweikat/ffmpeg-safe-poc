# MagicYUV: lossless video, Huffman-coded residuals of a left, gradient or
# median predictor. FFmpeg's decoder uses x86 assembly (up to AVX2) for the
# predictors.
C_DECODER=magicyuv
CONFIG=MAGICYUV
FATE_REAL="fate-magicyuv-rgb fate-magicyuv-rgba fate-magicyuv-y400i fate-magicyuv-y420 fate-magicyuv-y422i fate-magicyuv-y444 fate-magicyuv-y4444i"
FATE_ENC="fate-vsynth1-magicyuv fate-vsynth2-magicyuv fate-vsynth3-magicyuv fate-vsynth_lena-magicyuv"
FATE_SAMPLES="magy/magy_rgb_median.avi magy/magy_rgba_gradient.avi magy/magy_yuv400_gradient_interlaced.avi magy/magy_yuv420_median.avi magy/magy_yuv422_median_interlaced.avi magy/magy_yuv444_left.avi magy/magy_yuv4444_left_interlaced.avi lena.pnm"
# Packets carry their own format and size, so one seed file can hold clips
# of several formats: three files keep the number of fuzz processes down.
FUZZ_SEEDS="seed-mix-real.avi seed-mix-enc.avi seed-mix-small.avi"

# Join clips of this codec into one file, packets copied as they are.
mix() { # output name, input clip names...
    local out="$CLIPS/$CODEC/$1" list f
    shift
    [ -s "$out" ] && return
    list=$(mktemp)
    for f in "$@"; do echo "file '$CLIPS/$CODEC/$f'" >>"$list"; done
    $ff -f concat -safe 0 -i "$list" -c copy "$out" || { rm -f "$out"; echo "could not make $out"; }
    rm -f "$list"
}

make_clips() {
    # One clip per predictor, on the pixel formats FFmpeg's encoder writes.
    enc left-420-720p.avi      -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 40 -pix_fmt yuv420p -c:v magicyuv -pred left
    enc gradient-422-720p.avi  -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 40 -pix_fmt yuv422p -c:v magicyuv -pred gradient
    enc median-444-720p.avi    -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 40 -pix_fmt yuv444p -c:v magicyuv -pred median
    enc left-gbrp-720p.avi     -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 40 -pix_fmt gbrp -c:v magicyuv -pred left
    enc gradient-gbrap-720p.avi -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 30 -pix_fmt gbrap -c:v magicyuv -pred gradient
    enc median-gray-1080p.avi  -f lavfi -i "testsrc2=size=1920x1080:rate=30" -frames:v 40 -pix_fmt gray -c:v magicyuv -pred median
    # Photographic content, noisy: longer codes, fewer joint table hits.
    enc photo-left-704x576.avi     -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -frames:v 40 -c:v magicyuv -pred left
    enc photo-gradient-704x576.avi -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -frames:v 40 -c:v magicyuv -pred gradient
    enc photo-median-704x576.avi   -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -frames:v 40 -c:v magicyuv -pred median
    # Several slices, odd size.
    enc slices-median-637x353.avi  -f lavfi -i "testsrc2=size=637x353:rate=30" -frames:v 60 -pix_fmt yuv444p -c:v magicyuv -pred median -slices 5
    # Small clips for the fuzzer: every pixel format and predictor of the
    # encoder, one or several slices.
    enc seed-a.avi -f lavfi -i "testsrc2=size=98x62:rate=30"  -frames:v 6 -pix_fmt yuv420p  -c:v magicyuv -pred left -slices 3
    enc seed-b.avi -f lavfi -i "mandelbrot=size=64x48:rate=30" -frames:v 6 -pix_fmt gbrp     -c:v magicyuv -pred gradient
    enc seed-c.avi -f lavfi -i "testsrc2=size=96x48:rate=30"  -frames:v 6 -pix_fmt yuv444p  -c:v magicyuv -pred median -slices 2
    enc seed-d.avi -f lavfi -i "testsrc2=size=64x64:rate=30"  -frames:v 6 -pix_fmt yuv422p  -c:v magicyuv -pred median -slices 4
    enc seed-e.avi -f lavfi -i "testsrc2=size=80x40:rate=30"  -frames:v 6 -pix_fmt gbrap    -c:v magicyuv -pred left -slices 2
    enc seed-f.avi -f lavfi -i "mandelbrot=size=64x40:rate=30" -frames:v 6 -pix_fmt yuva444p -c:v magicyuv -pred gradient
    enc seed-g.avi -f lavfi -i "testsrc2=size=128x32:rate=30,noise=alls=40:allf=t" -frames:v 6 -pix_fmt gray -c:v magicyuv -pred median
    enc seed-h.avi -f lavfi -i "testsrc2=size=33x31:rate=30"  -frames:v 6 -pix_fmt yuv444p  -c:v magicyuv -pred median -slices 2
    enc seed-i.avi -f lavfi -i "testsrc2=size=18x14:rate=30"  -frames:v 6 -pix_fmt yuv420p  -c:v magicyuv -pred gradient -slices 2
    enc seed-j.avi -f lavfi -i "mandelbrot=size=9x7:rate=30"  -frames:v 6 -pix_fmt gbrp     -c:v magicyuv -pred median
    enc seed-k.avi -f lavfi -i "testsrc2=size=64x9:rate=30"   -frames:v 6 -pix_fmt gray     -c:v magicyuv -pred left -slices 3
    enc seed-l.avi -f lavfi -i "testsrc2=size=7x33:rate=30,noise=alls=80:allf=t" -frames:v 6 -pix_fmt yuv444p -c:v magicyuv -pred gradient
    real magy/magy_rgb_median.avi                  real-rgb-median.avi
    real magy/magy_rgba_gradient.avi               real-rgba-gradient.avi
    real magy/magy_yuv400_gradient_interlaced.avi  real-y400i-gradient.avi
    real magy/magy_yuv420_median.avi               real-y420-median.avi
    real magy/magy_yuv422_median_interlaced.avi    real-y422i-median.avi
    real magy/magy_yuv444_left.avi                 real-y444-left.avi
    real magy/magy_yuv4444_left_interlaced.avi     real-y4444i-left.avi
    mix seed-mix-real.avi  real-rgb-median.avi real-rgba-gradient.avi real-y400i-gradient.avi real-y420-median.avi \
                           real-y422i-median.avi real-y444-left.avi real-y4444i-left.avi
    mix seed-mix-enc.avi   seed-a.avi seed-b.avi seed-c.avi seed-d.avi seed-e.avi seed-f.avi seed-g.avi
    mix seed-mix-small.avi seed-h.avi seed-i.avi seed-j.avi seed-k.avi seed-l.avi
}

# Deliberately broken variants: "name|sed expression for magicyuv-core/src/*.rs".
MUTANTS=(
    "joint entry stores only its symbols|s/dst\[x..x + 8\].copy_from_slice(&elem.to_le_bytes());/dst[x..x + num].copy_from_slice(\&elem.to_le_bytes()[..num]);/"
    "no bits taken from past the slice|s/let rest = self.buf.get(self.ptr..).unwrap_or(&\[\]);/let rest = self.buf.get(self.ptr..self.end).unwrap_or(\&[]);/"
    "unused code gives symbol 0, not -1|s/e.sym = -1;/e.sym = 0;/"
    "AVX2 left: low half not carried|s/x = _mm256_add_epi8(x, low_total);/x = _mm256_add_epi8(x, _mm256_setzero_si256());/"
    "16-bit median: gradient masked|s/let grad = l + t - lt;/let grad = (l + t - lt) \& max as i32;/"
    "joint table: code below usable not tried|s/let t = if j == 1 { i - 1 } else { i };/let t = i;/"
)
