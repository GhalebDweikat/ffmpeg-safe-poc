# Cinepak video (AVI, QuickTime, Sega FILM).
C_DECODER=cinepak
CONFIG=CINEPAK
FATE_REAL="fate-cvid-palette fate-cvid-partial fate-cvid-grayscale fate-film-cvid fate-segafilm-cinepak-mux"
FATE_ENC="fate-vsynth1-cinepak fate-vsynth2-cinepak fate-vsynth_lena-cinepak"
FATE_SAMPLES="cvid/laracroft-cinepak-partial.avi cvid/catfight-cvid-pal8-partial.mov cvid/pcitva15.avi film/logo-capcom.cpk lena.pnm"
FUZZ_SEEDS="real-laracroft.avi real-catfight-pal8.mov real-pcitva15-grey.avi seed-testsrc-160x120.avi seed-mandel-92x60.avi"

make_clips() {
    # FFmpeg's Cinepak encoder is slow, so these clips are short. It also
    # only takes sizes that are multiples of 4; odd sizes come from the
    # fuzzer's generated streams instead.
    enc testsrc2-720p.avi   -f lavfi -i "testsrc2=size=1280x720:rate=30" -frames:v 40 -c:v cinepak
    enc mandelbrot-720p.avi -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 40 -c:v cinepak
    enc noise-640x360.avi   -f lavfi -i "testsrc2=size=640x360:rate=30,noise=alls=60:allf=t" -frames:v 60 -c:v cinepak
    enc small-636x352.avi   -f lavfi -i "testsrc2=size=636x352:rate=30" -frames:v 60 -c:v cinepak
    enc photo-704x576.avi   -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -c:v cinepak
    enc seed-testsrc-160x120.avi -f lavfi -i "testsrc2=size=160x120:rate=30" -frames:v 60 -c:v cinepak
    enc seed-mandel-92x60.avi    -f lavfi -i "mandelbrot=size=92x60:rate=30" -frames:v 60 -c:v cinepak
    real cvid/laracroft-cinepak-partial.avi   real-laracroft.avi
    real cvid/catfight-cvid-pal8-partial.mov  real-catfight-pal8.mov
    real cvid/pcitva15.avi                    real-pcitva15-grey.avi
}

# Deliberately broken variants: "name|sed expression for cinepak-core/src/lib.rs".
MUTANTS=(
    "block past the row end cut off at the row|s/frame.put((y + r) \* stride + x \* F::BPP, rows\[r\].as_ref());/let o = x * F::BPP; let n = frame.row_len.saturating_sub(o).min(rows[r].as_ref().len()); frame.put((y + r) * stride + o, \&rows[r].as_ref()[..n]);/"
    "V1 block: bottom half gets a top colour|s/let (lo, hi) = doubled(le32(p, 6) \& 0xff_ffff, le32(p, 8) >> 8);/let (lo, hi) = doubled(le32(p, 0) \& 0xff_ffff, le32(p, 8) >> 8);/"
    "codebooks not inherited from last strip|s/if i > 0 \&\& frame_flags \& 0x01 == 0 {/if false {/"
    "chroma halved by shift, not division|s/let d = \[w \* 2, -(u \/ 2) - w, u \* 2\];/let d = [w * 2, -(u >> 1) - w, u * 2];/"
    "relative strip position not wrapped|s/rect.y2 = (y0 + rect.y2) \& 0xffff;/rect.y2 = y0 + rect.y2;/"
    "Sega FILM extra header bytes ignored|s/let mut data = packet.get(10 + skip..)?;/let mut data = packet.get(10..)?;/"
)
