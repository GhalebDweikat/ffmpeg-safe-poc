# Microsoft RLE video (AVI).
C_DECODER=msrle
CONFIG=MSRLE
FATE_REAL="fate-msrle-8bit"
FATE_ENC="fate-vsynth1-msrle fate-vsynth2-msrle fate-vsynth3-msrle fate-vsynth_lena-msrle"
FATE_SAMPLES="msrle/Search-RLE.avi lena.pnm"
FUZZ_SEEDS="real-Search-RLE.avi seed-testsrc-160x120.avi seed-mandel-96x64.avi seed-life-65x47.avi"

make_clips() {
    # FFmpeg's encoder writes 8-bit palette video only.
    enc testsrc2-720p.avi   -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 300 -c:v msrle
    enc mandelbrot-720p.avi -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 300 -c:v msrle
    enc noise-720p.avi      -f lavfi -i "testsrc2=size=1280x720:rate=30,noise=alls=60:allf=t" -frames:v 120 -c:v msrle
    enc life-720p.avi       -f lavfi -i "life=size=1280x720:mold=10:rate=30:ratio=0.1:death_color=#102030:life_color=#f0e0d0" -frames:v 300 -c:v msrle
    enc small-636x352.avi   -f lavfi -i "testsrc2=size=636x352:rate=30" -frames:v 300 -c:v msrle
    enc photo-704x576.avi   -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -c:v msrle
    enc seed-testsrc-160x120.avi -f lavfi -i "testsrc2=size=160x120:rate=30" -frames:v 120 -c:v msrle
    enc seed-mandel-96x64.avi    -f lavfi -i "mandelbrot=size=96x64:rate=30" -frames:v 120 -c:v msrle
    enc seed-life-65x47.avi      -f lavfi -i "life=size=65x47:mold=10:rate=30:ratio=0.2" -frames:v 120 -c:v msrle
    real msrle/Search-RLE.avi real-Search-RLE.avi
}

# Deliberately broken variants: "name|sed expression for msrle-core/src/lib.rs".
MUTANTS=(
    "run bounded by width, not stride|s/let row_cap = frame.stride;/let row_cap = frame.row_len;/"
    "8-bit copy padding not skipped|s/if BPP == 1 \&\& p2 \& 1 != 0 {/if false {/"
    "skipped run consumes its pixel|s/let end = (pos + p1) \* BPP;/let end = (pos + p1) * BPP; if end > row_cap { next_byte(\&mut data); }/"
    "4-bit run starts on low nibble|s/let pair = \[b >> 4, b \& 0x0f\];/let pair = [b \& 0x0f, b >> 4];/"
    "raw picture not flipped|s/let src = \&packet\[(height - 1 - y) \* istride..\]\[..istride\];/let src = \&packet[y * istride..][..istride];/"
)
