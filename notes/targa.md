# targa: Truevision Targa image decoder

Port of `ffmpeg/libavcodec/targa.c` (312 lines, 247 of code) to `rust/targa-core` (375 lines of code without the tests), registered as `targa_rs`.

## Summary

- **FFmpeg's tests:** 12 of 12 pass with the port as the default decoder (11 real pictures: `fate-targa-conformance-*` and `fate-targa-top-to-bottom`; 1 through the encoder: `fate-lavf-tga`). They passed on the first build. `results/targa-fate.txt`.
- **Differential fuzz:** 127,085,228 packets, 0 mismatches, 0 panics (`results/targa-fuzz.txt`, 5 processes for 420 s). This run includes a comparison of the palette and of frames left behind by refused packets, which the shared harness does not make (see "Changes needed in shared files"). An earlier run of 155,967,850 packets without that comparison was also clean (`results/targa-fuzz-before-palette-check.txt`).
- **Sanitizer fuzz (ASan + UBSan on the C code):** 9,246,776 packets, clean (`results/targa-fuzz-asan.txt`).
- **Benchmark, Rust time / C time:**
  - Straight port: clips 0.98x to 3.33x, single paths 0.89x to 3.61x (`results/targa-bench-v1.txt`).
  - Final: clips 0.10x to 1.02x, single paths 0.03x to 1.02x (`results/targa-bench-final.txt`). No row is above 1.1. The rows near 1.0 are the uncompressed pictures, where both decoders are one `memcpy` per row.
  - Measured while other ports were running on the machine; the C times moved by up to 40% between runs.
- **Versions:** 2 (straight port, then one optimised version).
- **Bugs in the port:** none found in the Rust decoder by any check. Two things were wrong around it:
  - The fuzzer was not comparing palettes. Caught by the broken-variant table: the variant "16-bit palette: low bits not filled" was missed by the fuzz. Fixed inside `harness/codec_targa.c`; all 8 variants are now caught by 10 s of fuzz (`results/targa-mutants.txt`: unit tests 7 of 8, FFmpeg's real samples 1 of 8, FFmpeg's encoder test 0 of 8, fuzz 8 of 8).
  - My first fuzz setup refused to open the decoders in 1 of 8 cases (seen as `refused_configs` in the output), and two clips meant to have odd sizes were even (`testsrc2` rounds sizes; seen with ffprobe). Both fixed before any saved result.
- **C behaviour copied:** see "What the C decoder does that the port copies" below. In short: a cut-short literal packet leaves old frame contents in the picture; the palette is written before the pixels, so a refused picture still changes the palette of a buffer that is handed out again; 15-bit pictures are not mirrored; the order of the checks and of the side effects on the codec context.
- **Wrong in the C decoder:** a run packet whose pixel value is cut off by the end of the data makes it repeat bytes of an uninitialised stack array (`uint8_t tmp[4]` in `targa_decode_rle`). Reproduce with `results/targa-uninit-repro.tga` (24 bytes): the C decoder outputs `11 bb cc` for the cut-off pixel, where `bb cc` are left over from the run before; the port outputs `11 00 00`. Details below. Not reported upstream.
- **Not ported or not covered:** log messages are shorter (no numbers in them). Pictures above 524,288 pixels are not fuzzed. A caller's own `get_buffer2` returning a negative line size is refused by the shim with `AVERROR_BUG`, where the C decoder would decode. `fate-seek-lavf-tga` is not run (it does not decode). Big-endian hosts are untested.
- **Changes needed in shared files:** `harness/common.c`, `plane_extent()`: test for the palette before `p >= planes`, so that `frame_diff` and `frame_hash` cover the palette of pal8 frames. Until then no decoder's palette is compared by the fuzzer or the benchmark. Also: `scripts/asan.sh` was edited by someone else during my first sanitizer run, which died with a shell syntax error; the saved output is from the second run.
- **Not verified:** benchmark numbers on an idle machine. The sanitizer run never saw the uninitialised read, because the fuzzer avoids that input and ASan/UBSan do not detect such reads anyway; the finding rests on reading the code and on the observed output, not on MSan or valgrind (neither is in the container). Whether upstream FFmpeg after 8.1.3 still has it.

## Files

| File | Lines |
|---|---|
| `rust/targa-core/src/lib.rs` | 375 of code, plus 11 unit tests |
| `rust/ffsafe-ffi/src/targa.rs` | 158 |
| `shim/targa_rs.c` | 142 |
| `harness/codec_targa.c` | about 475 |
| `scripts/codecs/targa.sh` | 71 |

## How it is wired

A Targa packet carries its own picture size and pixel format, so the shim cannot allocate a frame before Rust has looked at the packet. There are two entry points:

- `targa_rs_header(pkt, size, &info)` parses the header and says which of the C decoder's early exits applies, plus format, width and height.
- `targa_rs_decode(pkt, size, frame, frame_size, stride, pal)` parses the header again, checks the frame it was given against it (it does not trust the shim for the size), and decodes. `pal` is the 1024-byte palette plane of a pal8 frame.

The shim makes the FFmpeg calls in the order the C decoder makes them (set pixel format, `ff_set_dimensions`, `ff_get_buffer`), because a caller sees their effects even when the packet is then refused.

## Versions

| Version | What changed | Clips, Rust/C | Paths, Rust/C |
|---|---|---|---|
| v1 straight port (`results/targa-lib-v1-straight-port.rs`) | | 0.98 to 3.33 | 0.89 to 3.61 |
| v2 final (`results/targa-lib-v2-final.rs`) | bytes per pixel as a const generic; a slice of what is left of the current row; fixed-size pixel stores; rows mirrored with `reverse()` on pixel chunks | 0.10 to 1.02 | 0.03 to 1.02 |

In v1 the slow rows were the run packets (2.2x to 3.6x) and the mirroring (2.0x to 3.4x): both copied one pixel at a time through a slice of run-time length, which is a `memcpy` call per pixel. Uncompressed pictures and literal packets were at parity from the start. v2 was measured twice (`results/targa-bench-v2.txt`, `results/targa-bench-final.txt`); the largest ratio in either is 1.10 (median of an uncompressed path whose minimum was 0.99), which is noise on a shared machine.

The C decoder's run loop is `memcpy(dst, tmp, depth)` per pixel with a run-time `depth`, which is why the port is 2x to 30x faster on runs. The same change would speed up the C.

## What the C decoder does that the port copies

- **Checked header reads.** The 18 header bytes are read with FFmpeg's checked reader: a field that does not fit reads as zero, and a 16-bit field that does not fit also swallows the byte before it. A 17-byte packet is a valid header with flags 0.
- **Order of checks and side effects.** Header too short for identifier plus 2 bytes per colour (whatever the entry size) is refused first; then the bit depth; then the pixel format is set on the context; then the colour map bound (more than 256 entries); then the dimensions are set; then "no picture" types (0 and 8) return success without a frame; then an uncompressed picture larger than the packet is refused; only then is the frame allocated. The colour map entry size is checked after allocation.
- **Colour map fields without a colour map are ignored** (with a warning), as the specification says.
- **8-bit pictures are gray only for type 3 or 11;** any other type gives pal8, with or without a colour map.
- **The palette is written before the pixels.** If the pixels then fail, the palette of that frame buffer has still changed. FFmpeg's frame pool hands the same buffer to a later picture of the same size and format, and a pal8 picture with fewer than 256 colours (or none) shows the old entries.
- **A literal packet cut short by the end of the data** copies the bytes that are there, leaves the remaining pixels as they were, and still counts them. If that packet was the last one, the picture is output with old buffer contents in it.
- **A run or literal packet may continue from one row into the next.** A packet that runs past the last row is an error ("Packet went out of bounds"), but the rows it filled were written.
- **Mirroring.** With the right-to-left flag, rows are mirrored after decoding, for 8, 16, 24 and 32 bits. The C `switch` has no case for 15, so 15-bit pictures are not mirrored.
- **Interleaving.** Two-way is tested before four-way; with both flags set it is two-way. The row order (0, n, 2n.., then 1, n+1..) follows `advance_line`, including pictures with fewer rows than passes.
- **Two size checks for uncompressed pictures:** one before allocation that ignores the colour map, one after the colour map was read.

The C decoder never writes outside the visible rows, so nothing had to be dropped under rule 3.

## Wrong in the C decoder: uninitialised bytes in a cut-off run

`targa_decode_rle`, run packets:

```c
uint8_t tmp[4];
bytestream2_get_buffer(&s->gb, tmp, depth);
```

`bytestream2_get_buffer` copies only as many bytes as the packet has left. If the data ends inside the pixel value, the rest of `tmp` is whatever is on the stack, and the loop below writes it into the frame `count` times. If the run completes the picture exactly, the picture is output with those bytes; if not, the packet is refused, but the bytes are in a frame buffer that FFmpeg reuses.

Reproduce (`results/targa-uninit.txt`):

```
ffmpeg -c:v targa    -i results/targa-uninit-repro.tga -f rawvideo -pix_fmt bgr24 - | od -A n -t x1
 aa bb cc aa bb cc 11 bb cc 11 bb cc
ffmpeg -c:v targa_rs -i results/targa-uninit-repro.tga -f rawvideo -pix_fmt bgr24 -
 aa bb cc aa bb cc 11 00 00 11 00 00
```

The file is a 4x1, 24-bit picture: a run of 2 with pixel `aa bb cc`, then a run of 2 whose pixel value is cut off after `11`. In this build the stale bytes are those of the previous run; for the first run of a packet they are whatever earlier calls left on the stack (a file with only the cut-off run gave zeros from the `ffmpeg` program; I did not look at what the fuzzer's process gives in that case).

The port reads the missing bytes as zero. The fuzzer keeps away from exactly this input: `avoid_uninit()` in `harness/codec_targa.c` follows the decoder through each packet and, when the data ends inside a run's pixel value, appends the missing bytes (at most 3). Packets that end right after a complete pixel value, cut-short literal packets and everything else are left alone. Setting `TARGA_FUZZ_UNINIT=1` in the environment turns the avoidance off; the fuzzer then reports a mismatch within about 2,000 cases on every seed tried.

ASan and UBSan do not detect reads of uninitialised memory, and neither MSan nor valgrind is in the container, so no tool has confirmed this.

## The fuzzer

`harness/codec_targa.c` generates pictures of every bit depth (and unsupported ones), every type, all flag combinations, colour maps of every entry size with offsets, identifiers, packets that cross rows, last packets that are too long, literal packets cut short, and uncompressed pictures a few bytes too short or long. About one packet in twelve has a different size from the others of its case, so the frame pool is rebuilt mid-stream. Seeds are the eleven real pictures and fifty small encoded ones (five pixel formats, two sizes, compressed and not), each set in one NUT file.

Two things are specific to this decoder:

- **Picture size bound.** A damaged header can ask for up to 268 million pixels, which both decoders would allocate before reading any data. The fuzzer sets `max_pixels` to 524,288 (in 1 case of 8 to 4,096, so that ordinary sizes are refused when the dimensions are set).
- **Second comparison.** `frame_diff` in `harness/common.c` does not reach the palette: `plane_extent` returns at `p >= planes`, and `av_pix_fmt_count_planes` is 1 for pal8. It also never compares a frame when the packet was refused. In half of the fuzz cases both decoders therefore allocate through `get_buffer()` in `codec_targa.c`, which keeps a reference to each decoder's last frame and, when the C decoder asks for its next one, requires the two frames left behind to be equal: every visible sample and the palette, accepted or not. The other half leave allocation alone, because the extra reference delays the reuse of a buffer by one frame. A mismatch found this way prints `MISMATCH what="..."` without a case number; rerun the seed to find it.

## Broken variants

`results/targa-mutants.txt`, 8 variants:

| Caught by | Count |
|---|---|
| Rust unit tests | 7 |
| FFmpeg's tests on real samples | 1 |
| FFmpeg's test through the encoder | 0 |
| 10 s of differential fuzz | 8 |

FFmpeg's tests caught only the palette variant. They miss the rest because no sample is 15-bit and mirrored, interleaved, cut short, or has a colour map description without a colour map.

## Not covered

- Pictures above 524,288 pixels in the fuzzer. The 1280x720 benchmark clips decode identically, but the benchmark's check does not include the palette either.
- A caller's `get_buffer2` that returns a negative line size: the shim returns `AVERROR_BUG`.
- Log messages: same events, shorter text, no numbers.
- `fate-seek-lavf-tga`: reads the files `fate-lavf-tga` makes without decoding them; not run.
- Big-endian hosts. The palette is written as native-endian 32-bit values, as in C, but this was only run on x86-64.
