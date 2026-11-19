# Microsoft Video 1 (AVI; also called CRAM).
C_DECODER=msvideo1
CONFIG=MSVIDEO1
FATE_REAL="fate-msvideo1-8bit fate-msvideo1-16bit fate-lavf-fate-cram.avi"
FATE_ENC="fate-vsynth1-msvideo1 fate-vsynth2-msvideo1 fate-vsynth_lena-msvideo1"
FATE_SAMPLES="cram/skating.avi cram/clock-cram16.avi cram/toon.avi lena.pnm"
# Three seed clips by default, so that a fuzz run with three generating
# processes is six processes. MSVIDEO1_SEEDS=more selects the other two.
FUZZ_SEEDS="real-skating-pal8.avi real-clock-rgb555.avi seed-testsrc-160x120.avi"
[ "${MSVIDEO1_SEEDS:-}" = more ] && FUZZ_SEEDS="real-toon-pal8.avi seed-mandel-92x60.avi"

make_clips() {
    # FFmpeg's encoder writes 16-bit video only, and only at sizes that are
    # multiples of 4. Palette video comes from the real samples and from the
    # generated streams (harness/codec_msvideo1.c).
    enc testsrc2-720p.avi   -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 60 -c:v msvideo1
    enc mandelbrot-720p.avi -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 60 -c:v msvideo1
    enc noise-640x360.avi   -f lavfi -i "testsrc2=size=640x360:rate=30,noise=alls=60:allf=t" -frames:v 60 -c:v msvideo1
    enc life-720p.avi       -f lavfi -i "life=size=1280x720:mold=10:rate=30:ratio=0.1:death_color=#102030:life_color=#f0e0d0" -frames:v 60 -c:v msvideo1
    enc small-636x352.avi   -f lavfi -i "testsrc2=size=636x352:rate=30" -frames:v 60 -c:v msvideo1
    enc photo-704x576.avi   -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -c:v msvideo1
    enc seed-testsrc-160x120.avi -f lavfi -i "testsrc2=size=160x120:rate=30" -frames:v 60 -c:v msvideo1
    enc seed-mandel-92x60.avi    -f lavfi -i "mandelbrot=size=92x60:rate=30" -frames:v 60 -c:v msvideo1
    real cram/skating.avi      real-skating-pal8.avi
    real cram/clock-cram16.avi real-clock-rgb555.avi
    real cram/toon.avi         real-toon-pal8.avi
}

# Deliberately broken variants: "name|sed expression for msvideo1-core/src/lib.rs".
MUTANTS=(
    "skip count 0 skips one block only|s/skip = (i32::from(byte_b - 0x84) << 8) + i32::from(byte_a) - 1;/skip = ((i32::from(byte_b - 0x84) << 8) + i32::from(byte_a) - 1).max(0);/"
    "skip run one block too long|s/ + i32::from(byte_a) - 1;/ + i32::from(byte_a);/"
    "palette 8-colour code from 0x88|s/} else if byte_b >= 0x90 {/} else if byte_b >= 0x88 {/"
    "16-bit 8-colour: top uses bottom's|s/(top & EVEN) \* 0x0001_0001\];/(bottom \& EVEN) * 0x0001_0001];/"
    "cut-off packet still sets palette|s/return Outcome::Truncated;/return Outcome::Finished;/"
    "16-bit 1-colour loses its top bit|s/Some(\[Self::row(u64::from(flags) \* ONES); 4\])/Some([Self::row(u64::from(flags \& 0x7FFF) * ONES); 4])/"
    "packet size check off by one|s/len >= (width \/ 4)/len > (width \/ 4)/"
    "skipped block row not counted|s/skip -= blocks_wide as i32;/skip -= 0;/"
)
