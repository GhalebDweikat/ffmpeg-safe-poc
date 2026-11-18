# Apple RPZA ("Road Pizza") video.
C_DECODER=rpza
CONFIG=RPZA
FATE_REAL="fate-rpza"
FATE_ENC="fate-vsynth1-rpza fate-vsynth2-rpza fate-vsynth3-rpza fate-vsynth_lena-rpza"
FATE_SAMPLES="rpza/rpza2.mov lena.pnm"
FUZZ_SEEDS="real-rpza2.mov odd-637x353.mov small-636x352.mov seed-mandel-160x120.mov seed-noise-96x64.mov"

make_clips() {
    enc testsrc2-720p.mov   -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 300 -c:v rpza
    enc mandelbrot-720p.mov -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 300 -c:v rpza
    enc noise-720p.mov      -f lavfi -i "testsrc2=size=1280x720:rate=30,noise=alls=60:allf=t" -frames:v 120 -c:v rpza
    enc life-720p.mov       -f lavfi -i "life=size=1280x720:mold=10:rate=30:ratio=0.1:death_color=#102030:life_color=#f0e0d0" -frames:v 300 -c:v rpza
    enc small-636x352.mov   -f lavfi -i "testsrc2=size=636x352:rate=30" -frames:v 300 -c:v rpza
    # Width and height that are not multiples of 4, to exercise edge blocks.
    enc odd-637x353.mov     -f lavfi -i "testsrc2=size=640x360:rate=30,format=rgb555le,scale=637:353:flags=neighbor" -frames:v 300 -c:v rpza
    # Small clips whose packets the fuzzer mutates.
    enc seed-mandel-160x120.mov -f lavfi -i "mandelbrot=size=160x120:rate=30" -frames:v 120 -c:v rpza
    enc seed-noise-96x64.mov    -f lavfi -i "testsrc2=size=96x64:rate=30,noise=alls=60:allf=t" -frames:v 120 -c:v rpza
    # Photo-like content: FATE's rotating-photo source, scaled up.
    enc photo-704x576.mov   -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -c:v rpza
    real rpza/rpza2.mov real-rpza2.mov
}

# Deliberately broken variants: "name|sed expression for rpza-core/src/lib.rs".
MUTANTS=(
    "blend weight 21 -> 20|s/let c1 = gather(11 \* sa + 21 \* sb);/let c1 = gather(11 * sa + 20 * sb);/"
    "skip run of 32 treated as 31|s/0x80 => w.advance(n_blocks),/0x80 => w.advance(n_blocks.min(31)),/"
    "16-colour block: first pixel|s/let mut px = \[color_a; 16\];/let mut px = [color_a ^ 1; 16];/"
    "extra block after frame accepted|s/if w.remaining == 0 {/if false {/"
    "edge column loses a byte|s/for (d, s) in edge.iter_mut().zip(src) {/for (d, s) in edge.iter_mut().zip(src).skip(1) {/"
)
