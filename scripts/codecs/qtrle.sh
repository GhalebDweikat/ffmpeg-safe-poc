# QuickTime Animation (RLE) video.
C_DECODER=qtrle
CONFIG=QTRLE
FATE_REAL="fate-qtrle-1bit fate-qtrle-2bit fate-qtrle-4bit fate-qtrle-8bit fate-qtrle-16bit fate-qtrle-24bit fate-qtrle-32bit fate-rgb24-mkv fate-filter-fps fate-filter-fps-cfr"
FATE_ENC="fate-vsynth1-qtrle fate-vsynth2-qtrle fate-vsynth3-qtrle fate-vsynth_lena-qtrle fate-vsynth1-qtrlegray fate-vsynth2-qtrlegray fate-vsynth_lena-qtrlegray"
FATE_SAMPLES="qtrle/aletrek-rle.mov qtrle/Animation-Monochrome.mov qtrle/Animation-4Greys.mov qtrle/Animation-16Greys.mov qtrle/criticalpath-credits.mov qtrle/mr-cork-rle.mov qtrle/ultra_demo_720_480_32bpp_rle.mov qtrle/apple-animation-variable-fps-bug.mov lena.pnm"
# Every clip whose packets the fuzzer mutates gets its own process, so the
# default is short. QTRLE_SEEDS picks others: any real-*.mov or seed-*.mov
# made below.
FUZZ_SEEDS=${QTRLE_SEEDS:-"real-1bit.mov real-4bit.mov seed-mandel-96x64-rgb555.mov"}

make_clips() {
    # FFmpeg's encoder writes 8-bit grey (as palette video), 16, 24 and 32
    # bits. 1, 2 and 4 bits exist only as real samples.
    enc testsrc2-720p-rgb24.mov  -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 300 -c:v qtrle -pix_fmt rgb24
    enc testsrc2-720p-rgb555.mov -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 300 -c:v qtrle -pix_fmt rgb555be
    enc testsrc2-720p-argb.mov   -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 300 -c:v qtrle -pix_fmt argb
    enc testsrc2-720p-gray.mov   -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 300 -c:v qtrle -pix_fmt gray
    enc mandelbrot-720p-rgb24.mov -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 90 -c:v qtrle -pix_fmt rgb24
    enc mandelbrot-720p-rgb555.mov -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 90 -c:v qtrle -pix_fmt rgb555be
    enc noise-720p-rgb24.mov     -f lavfi -i "testsrc2=size=1280x720:rate=30,noise=alls=60:allf=t" -frames:v 40 -c:v qtrle -pix_fmt rgb24
    enc noise-720p-argb.mov      -f lavfi -i "testsrc2=size=1280x720:rate=30,noise=alls=60:allf=t" -frames:v 40 -c:v qtrle -pix_fmt argb
    enc noise-720p-gray.mov      -f lavfi -i "testsrc2=size=1280x720:rate=30,noise=alls=60:allf=t" -frames:v 60 -c:v qtrle -pix_fmt gray
    enc life-720p-gray.mov       -f lavfi -i "life=size=1280x720:mold=10:rate=30:ratio=0.1:death_color=#102030:life_color=#f0e0d0" -frames:v 150 -c:v qtrle -pix_fmt gray
    enc life-720p-argb.mov       -f lavfi -i "life=size=1280x720:mold=10:rate=30:ratio=0.1:death_color=#102030:life_color=#f0e0d0" -frames:v 150 -c:v qtrle -pix_fmt argb
    enc odd-637x353-rgb555.mov   -f lavfi -i "testsrc2=size=640x360:rate=30,scale=637:353:flags=neighbor" -frames:v 300 -c:v qtrle -pix_fmt rgb555be
    enc photo-704x576-rgb24.mov  -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -c:v qtrle -pix_fmt rgb24
    enc photo-704x576-gray.mov   -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -c:v qtrle -pix_fmt gray
    # Small clips whose packets the fuzzer mutates.
    enc seed-testsrc-160x120-rgb24.mov -f lavfi -i "testsrc2=size=160x120:rate=30" -frames:v 120 -c:v qtrle -pix_fmt rgb24
    enc seed-mandel-96x64-rgb555.mov   -f lavfi -i "mandelbrot=size=96x64:rate=30" -frames:v 120 -c:v qtrle -pix_fmt rgb555be
    enc seed-life-68x47-gray.mov       -f lavfi -i "life=size=68x47:mold=10:rate=30:ratio=0.2" -frames:v 120 -c:v qtrle -pix_fmt gray
    enc seed-noise-97x63-argb.mov      -f lavfi -i "testsrc2=size=97x63:rate=30,noise=alls=40:allf=t" -frames:v 120 -c:v qtrle -pix_fmt argb
    real qtrle/Animation-Monochrome.mov              real-1bit.mov
    real qtrle/Animation-4Greys.mov                  real-2bit.mov
    real qtrle/Animation-16Greys.mov                 real-4bit.mov
    real qtrle/criticalpath-credits.mov              real-8bit.mov
    real qtrle/mr-cork-rle.mov                       real-16bit.mov
    real qtrle/aletrek-rle.mov                       real-24bit.mov
    real qtrle/ultra_demo_720_480_32bpp_rle.mov      real-32bit.mov
}

# Deliberately broken variants: "name|sed expression for qtrle-core/src/lib.rs".
MUTANTS=(
    "16-bit copy not byte-swapped|s/            \*d = u16::from_be_bytes(\*s).to_ne_bytes();/            *d = *s;/"
    "1-bit: extra line not counted|s/let mut lines = lines as i64 + 1;/let mut lines = lines as i64;/"
    "limit is the visible end, not stride x height|s/(self.stride \* self.height) as i64/(self.stride * (self.height - 1) + self.row_len) as i64/"
    "skip code off by one|s/x += out \* (next_byte(\&mut data) as i64 - 1);/x += out * (next_byte(\&mut data) as i64);/"
    "discard_damaged_percentage fixed at 95|s/size \* discard_damaged_percentage as i64 \/ 100/size * 95 \/ 100/"
    "24-bit short packet: zeros per byte, not group|s/let b: \[u8; 2\] = r.array();/let b = [r.byte(), r.byte()];/"
    "write stops at the row end, not continued|s/            bytes = rest;/            bytes = \&[];/"
    "4-bit pixels swapped|s/    \[x >> 4, x \& 0x0f\]/    [x \& 0x0f, x >> 4]/"
)
