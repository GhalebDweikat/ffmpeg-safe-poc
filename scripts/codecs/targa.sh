# Truevision Targa image.
C_DECODER=targa
CONFIG=TARGA
# The ten conformance images are 8-bit gray, 8-bit colour-mapped, and 16-,
# 24- and 32-bit true colour, each uncompressed (U) and run-length coded (C).
FATE_REAL="fate-targa-conformance-CBW8 fate-targa-conformance-CCM8 fate-targa-conformance-CTC16 fate-targa-conformance-CTC24 fate-targa-conformance-CTC32 fate-targa-conformance-UBW8 fate-targa-conformance-UCM8 fate-targa-conformance-UTC16 fate-targa-conformance-UTC24 fate-targa-conformance-UTC32 fate-targa-top-to-bottom"
# Encodes 13 pictures with FFmpeg's Targa encoder and decodes them again.
# (fate-seek-lavf-tga reads the same files but does not decode them.)
FATE_ENC="fate-lavf-tga"
FATE_SAMPLES="targa-conformance/CTC24.TGA targa-conformance/CBW8.TGA targa-conformance/CCM8.TGA targa-conformance/CTC16.TGA targa-conformance/CTC32.TGA targa-conformance/UBW8.TGA targa-conformance/UCM8.TGA targa-conformance/UTC16.TGA targa-conformance/UTC24.TGA targa-conformance/UTC32.TGA targa/lena-top-to-bottom.tga"
# One file with the eleven real pictures, one with small encoded pictures of
# every pixel format, compressed and not. A Targa packet carries its own
# size and format, so one stream can hold pictures of different kinds.
FUZZ_SEEDS="seed-real.nut seed-encoded.nut"
DEMUXERS="nut"

make_clips() {
    local d="$CLIPS/$CODEC" f s
    local t2="testsrc2=size=1280x720:rate=30"
    # Run-length coded (the encoder's default), every pixel format.
    enc testsrc2-720p-bgr24.nut  -f lavfi -i "$t2" -frames:v 120 -pix_fmt bgr24 -c:v targa
    enc testsrc2-720p-bgra.nut   -f lavfi -i "$t2" -frames:v 120 -pix_fmt bgra -c:v targa
    enc testsrc2-720p-rgb555.nut -f lavfi -i "$t2" -frames:v 120 -pix_fmt rgb555le -c:v targa
    enc testsrc2-720p-gray.nut   -f lavfi -i "$t2" -frames:v 120 -pix_fmt gray -c:v targa
    enc testsrc2-720p-pal8.nut   -f lavfi -i "$t2" -frames:v 120 -pix_fmt pal8 -c:v targa
    enc mandelbrot-720p-bgr24.nut -f lavfi -i "mandelbrot=size=1280x720:rate=30" -frames:v 120 -pix_fmt bgr24 -c:v targa
    enc life-720p-gray.nut       -f lavfi -i "life=size=1280x720:mold=10:rate=30:ratio=0.1:death_color=#102030:life_color=#f0e0d0" -frames:v 120 -pix_fmt gray -c:v targa
    # A photo: few runs, mostly literal packets.
    enc photo-704x576-bgr24.nut  -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -pix_fmt bgr24 -c:v targa
    enc photo-704x576-bgra.nut   -f rawvideo -pix_fmt yuv420p -s 352x288 -i "$SRC/tests/data/vsynth2.yuv" -vf scale=704:576 -pix_fmt bgra -c:v targa
    # An odd width (testsrc2 only makes even sizes).
    enc small-321x240-bgr24.nut  -f lavfi -i "testsrc=size=321x240:rate=30" -frames:v 300 -pix_fmt bgr24 -c:v targa
    # Uncompressed.
    enc raw-720p-bgr24.nut       -f lavfi -i "$t2" -frames:v 30 -pix_fmt bgr24 -c:v targa -rle 0
    enc raw-720p-bgra.nut        -f lavfi -i "$t2" -frames:v 30 -pix_fmt bgra -c:v targa -rle 0
    enc raw-720p-pal8.nut        -f lavfi -i "$t2" -frames:v 30 -pix_fmt pal8 -c:v targa -rle 0
    # The real conformance pictures, 128x128 each, 200 times over.
    for f in CTC24 CCM8 UTC32; do
        enc real-$f-x200.nut -loop 1 -i "$SAMPLES/targa-conformance/$f.TGA" -frames:v 200 -c:v copy
    done

    # Fuzz seeds.
    : >"$d/seed-real.txt"
    for f in $FATE_SAMPLES; do
        echo "file '$SAMPLES/$f'" >>"$d/seed-real.txt"
    done
    enc seed-real.nut -f concat -safe 0 -i "$d/seed-real.txt" -c:v copy
    : >"$d/seed-encoded.txt"
    for f in bgr24 bgra rgb555le gray pal8; do
        for s in 65x47 32x7; do
            enc seed-part-$f-$s-rle.nut -f lavfi -i "testsrc=size=$s:rate=30" -frames:v 3 -pix_fmt $f -c:v targa
            enc seed-part-$f-$s-raw.nut -f lavfi -i "testsrc=size=$s:rate=30" -frames:v 2 -pix_fmt $f -c:v targa -rle 0
            echo "file '$d/seed-part-$f-$s-rle.nut'" >>"$d/seed-encoded.txt"
            echo "file '$d/seed-part-$f-$s-raw.nut'" >>"$d/seed-encoded.txt"
        done
    done
    enc seed-encoded.nut -f concat -safe 0 -i "$d/seed-encoded.txt" -c:v copy
}

# Deliberately broken variants: "name|sed expression for targa-core/src/lib.rs".
MUTANTS=(
    "15-bit pictures mirrored too|s/ \&\& hdr.bpp != 15 {/ {/"
    "interleave: later passes start a row late|s/let y = (y + 1) \& (self.interleave - 1);/let y = (y + 2) \& (self.interleave - 1);/"
    "missing literal pixels zeroed, not kept|s/dst\[..pixels.len()\].copy_from_slice(pixels);/dst.fill(0); dst[..pixels.len()].copy_from_slice(pixels);/"
    "16-bit palette: low bits not filled|s/0xff00_0000 | v | (v \& 0xe0e0e0) >> 5/0xff00_0000 | v/"
    "palette bound off by one|s/colors + first_clr > 256/colors + first_clr > 257/"
    "colour map fields kept without a map|s/if pal == 0 \&\& (first_clr != 0 || colors != 0 || csize != 0) {/if false {/"
    "pixels after the last row accepted|s/return if count != 0 { Err(DecodeError::RleOutOfBounds) } else { Ok(()) };/return Ok(());/"
    "raw size not checked after colour map|s/if (r.left() as u64) < hdr.raw_size() {/if false {/"
)
