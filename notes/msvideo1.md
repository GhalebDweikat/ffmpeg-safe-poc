# msvideo1 (Microsoft Video 1)

Port of `ffmpeg/libavcodec/msvideo1.c` (351 lines with comments), registered as `msvideo1_rs`.

## Summary

- **FFmpeg tests:** 6 of 6 pass with the port as the default decoder (`fate-msvideo1-8bit`, `fate-msvideo1-16bit`, `fate-lavf-fate-cram.avi`, `fate-vsynth1-msvideo1`, `fate-vsynth2-msvideo1`, `fate-vsynth_lena-msvideo1`). They passed on the first build.
- **Differential fuzz:** 377.1 million packets, 0 mismatches, 0 panics. That is 306.9 million in the main run (6 processes, 600 s) and 70.2 million in a second run with the other two seed clips (5 processes, 180 s).
- **The fuzzer does not compare palettes.** This is a bug in the shared `harness/common.c` (details below). The fuzz result therefore covers return codes, frame properties and pixel indices of 8-bit video, but not the palette attached to the frame.
- **Sanitizer fuzz:** 22.4 million packets, clean (6 processes, 180 s). `asan.sh` hung twice in `configure` before it ran; see "Shared files".
- **Benchmark, Rust/C decode time:**

  | | clips | per path |
  |---|---|---|
  | straight port (v1) | 0.71 to 0.98 | 0.41 to 1.02 |
  | final (v4) | 0.41 to 0.63 | 0.14 to 0.64 |

  The final table was measured with the machine nearly idle; v1 to v3 were measured while other agents ran.
- **Versions:** 4. v1 straight port, v2 row words, v3 branchless (rejected: slower on four of nine clips and on every single-type path), v4 final.
- **Bugs in the port:** none found by any check. FATE passed on the first build and the first fuzz run was clean.
- **Broken variants:** 8 built. Unit tests caught 8, FFmpeg's real-sample tests 3, its encoder tests 2, the differential fuzz 7. The one the fuzz missed ("cut-off packet still sets palette") is exactly the palette gap above.
- **C behaviour copied:**
  - A skip code with a count of 0 skips every remaining block of the frame.
  - When a packet runs out, the frame is output without the current palette.
  - 16-bit rows are counted in pixels (the stride in bytes is halved).
  - A 16-bit one-colour block stores its two code bytes as the pixel, top bit included.
  - Packets shorter than `(width/4) * (height/4) / 512` bytes are rejected.
- **Wrong in the C decoder:** nothing that touches memory. Three oddities, with reproductions below:
  - A cut-off first packet of a palette stream gives an all-black picture.
  - The skip count of 0 behaviour, and the size check that contradicts it.
  - An end-marker test that can never succeed.
- **Not ported or not covered:**
  - Negative line sizes: the shim returns `AVERROR_BUG`, where the C code would decode.
  - The big-endian byte swap for 16-bit pixels is written but has never run.
  - Size changes in mid-stream are not fuzzed.
  - There is no large 8-bit clip, because FFmpeg's encoder writes 16-bit only. 8-bit speed comes from two small real samples and the synthetic paths.
- **Changes needed in shared files:** `harness/common.c` (palette never compared or hashed) and `scripts/asan.sh` (no protection against a hung `configure` test). Neither was edited.
- **Not verified:**
  - Palette equality under fuzzing, as above. The evidence I have instead is the 8-bit FATE test, the unit tests, and 22 cut copies of the two palette samples decoded by both decoders to RGB with identical checksums. That last check is not saved in `results/`.
  - Anything on a quiet machine other than the final benchmark.
- `STATUS=wip` is still set in `scripts/codecs/msvideo1.sh`.

## Files

| File | Lines |
|---|---|
| `rust/msvideo1-core/src/lib.rs` | 534, of which 213 are decoder code (rest: comments, 14 unit tests) |
| `rust/ffsafe-ffi/src/msvideo1.rs` | 72 |
| `shim/msvideo1_rs.c` | 136 |
| `harness/codec_msvideo1.c` | 292 |
| `scripts/codecs/msvideo1.sh` | 40 |

The decoder keeps nothing between frames except the picture and the palette, both of which the shim owns, so the Rust side is two plain functions: `msvideo1_rs_check` (the size check, before a frame is requested) and `msvideo1_rs_decode`.

## Versions

| Version | Saved as | Clips | Paths | What changed |
|---|---|---|---|---|
| v1 | `results/msvideo1-lib-v1-straight-port.rs`, `msvideo1-bench-v1.txt` | 0.71 to 0.98 | 0.41 to 1.02 | Line-by-line port, one pixel at a time. |
| v2 | `msvideo1-lib-v2-row-words.rs`, `msvideo1-bench-v2.txt` | 0.43 to 0.67 | 0.15 to 0.64 | Four row slices per block row, cut into fixed-size blocks and zipped, so no bounds checks. Each block row is built as one 32- or 64-bit word from a 16-entry mask table. A whole block row is skipped at once when the skip count covers it. |
| v3 | `msvideo1-lib-v3-branchless-rejected.rs`, `msvideo1-bench-v3-rejected.txt` | 0.43 to 0.70 | 0.15 to 1.12 | One straight-line path for all painted block types, chosen by conditional moves. Rejected. |
| v4 | `msvideo1-lib-v4-final.rs`, `msvideo1-bench-final.txt` | 0.41 to 0.63 | 0.14 to 0.64 | v2, plus eight-colour blocks built from word loads instead of byte gathers. 8-bit eight-colour went from 0.62 to 0.46. |

Why v3 lost: with conditional moves, where the next block starts depends on a load and a select for every block, about 3.5 ns per block. With branches the CPU predicts the block type and does not wait. v3 was faster only on random block mixes (0.47 against 0.64 on `mixed-pal`); on single-type paths it was about 3 times slower than v2 (fill: 1.08 and 1.12 against 0.40 and 0.37). On the nine clips it was slower by 20% to 30% on four (mandelbrot, clock, small, testsrc2), faster by 10% to 13% on three (noise, skating, toon) and level on two. Encoded content is mostly more predictable than a random mix.

The slowest rows of the final version are the random mixes (`mixed-pal` 0.64, `mixed-rgb` 0.60), which are limited by branch misprediction in both decoders.

## C behaviour that had to be copied

- **Skip count 0.** The C code computes `skip_blocks = count - 1` and tests `if (skip_blocks)`. A count of 0 gives -1, which is counted down and never reaches 0, so every remaining block of the frame is skipped. The port keeps the counter signed and returns "finished" when it is negative at the start of a block row.
- **No palette when the packet runs out.** `msvideo1_decode_8bit` copies the palette into the frame as its last statement; the bounds-check macro returns before that. The Rust decoder reports "ran out" and the shim skips the copy. The frame then carries whatever palette it had: zeros on the first frame, the previous palette later.
- **Stride in pixels.** The 16-bit path uses `linesize / 2` pixels per row. The port rounds the row pitch down to an even number of bytes, so an odd stride gives the same offsets as in C (unit test `rgb_odd_stride_counts_rows_in_pixels`). FFmpeg's own allocator never produces an odd stride.
- **Edges.** Only whole 4x4 blocks are coded. Columns right of the last whole block and rows below it keep the previous frame's content (zeros at first). The C decoder writes nothing outside the visible picture, so there was no padding behaviour to copy.

## Oddities in the C decoder

None of these is a memory-safety problem, and the sanitizer run was clean.

1. **Cut-off first packet gives a black picture.** Reproduce with the FATE sample:

   ```
   head -c 6000 cram/skating.avi > cut.avi
   ffmpeg -v quiet -c:v msvideo1 -i cut.avi -frames:v 1 -pix_fmt rgb24 -f rawvideo - | od -An -tu1 -v | tr -s ' ' '\n' | grep . | sort -n | uniq -c
   ```

   The output is 57600 zero bytes. The first packet is 4926 bytes at offset 2056, so about 80% of it is there, and the decoded indices are not zero, but the frame has no palette. The port does the same.
2. **Skip count 0.** The packet `00 84` decodes as "skip the whole frame". The size check assumes the longest skip covers 1023 blocks, so for a picture of 1536 blocks or more it rejects this packet as too small although the decoder would have accepted it.
3. **Dead end-marker test.** `if (byte_a == 0 && byte_b == 0 && total_blocks == 0) return;` cannot fire: two code bytes are only read for a block not yet counted, so `total_blocks` is at least 1 there. Two zero bytes are a two-colour block with all flags clear. The encoder's trailing `00 00` is never read. The port leaves the test out (unit test `end_marker_is_a_two_colour_block`).

## Fuzzing

`harness/codec_msvideo1.c` generates block streams for both depths: skips (short runs, runs up to 1023, count 0), one-, two- and eight-colour blocks, streams that stop early or run long, the encoder's end marker, and random bytes. `config` moves most pictures under 4x4 up to a legal size (a few stay, to compare the refusal), and one case in 24 gets a picture of more than 1023 blocks so that the size check and long skips matter. Stream-header palettes are absent, full, or too short.

A direct-call check runs the Rust entry point on a buffer exactly the size of the visible picture, kept from packet to packet, with marker bytes in the row padding and a random stride. It must match the frame from libavcodec and leave the padding alone. Its counts are in the result lines (`finished`, `ran_out`, `too_small`).

Seed clips: the default set is three (real 8-bit, real 16-bit, one encoded), so `fuzz.sh msvideo1 N 3` is six processes. `MSVIDEO1_SEEDS=more` in the environment selects the other two (`real-toon-pal8.avi`, `seed-mandel-92x60.avi`).

## Broken variants

`results/msvideo1-mutants.txt`:

| Variant | Unit tests | FATE real | FATE encoder | Fuzz, 10 s |
|---|---|---|---|---|
| skip count 0 skips one block only | caught | missed | missed | caught, case 27 |
| skip run one block too long | caught | caught | caught | caught, case 2 |
| palette 8-colour code from 0x88 | caught | missed | missed | caught, case 2 |
| 16-bit 8-colour: top uses bottom's colours | caught | caught | caught | caught, case 1 |
| cut-off packet still sets palette | caught | missed | missed | **missed** |
| 16-bit 1-colour loses its top bit | caught | missed | missed | caught, case 1 |
| packet size check off by one | caught | missed | missed | caught, case 50 |
| skipped block row not counted | caught | caught | missed | caught, case 2 |

The missed one is a gap in the harness, not equivalent code; see the next section.

## Shared files

Nothing shared was edited. Two changes are needed:

1. **`harness/common.c`, `plane_extent`: the palette is never compared or hashed.** `av_pix_fmt_count_planes(AV_PIX_FMT_PAL8)` is 1, and the function returns at `p >= planes` before it reaches the palette case for plane 1. So `frame_diff` and `frame_hash` skip `data[1]` for every palette format. The fix is to test the palette flag before the plane count. This also affects the fuzz and benchmark results already recorded for Microsoft RLE and Cinepak palette video: the README's statement that the palette is compared is not true for them either.
2. **`scripts/asan.sh`: `configure` can hang.** With `vm.mmap_rnd_bits` at 32 in this container, a program built with gcc 12's AddressSanitizer fails to start about one time in three (13 of 40 in a quick count) and loops printing `DEADLYSIGNAL`. `fuzz.sh` retries such starts, but `configure` runs a sanitized test program with no timeout. It hung twice for me and wrote 5.9 GB and 1.4 GB into `ffbuild/config.log`. I killed my own stuck test process each time, deleted the log, and then configured `/build/w/msvideo1/ffmpeg-asan` with the same `configure` command in a retry loop under a file-size limit. It passed on the second try, and `asan.sh` then ran unchanged. `setarch -R` is not permitted in the container. One `/tmp/ffconf.*` directory may be left over from the killed runs.

## Not verified

- Palette contents under fuzzing (see above).
- The 22 cut-file comparisons are not in `results/`. The command: for `skating.avi` and `toon.avi` and cut points 2100, 3000, 4000, 6000, 7000, 7500, 9000, 9600, 12400, 20000 and 50000 bytes, decode with `-c:v msvideo1` and `-c:v msvideo1_rs` to `-pix_fmt rgb24 -f rawvideo` and compare checksums. All 22 matched.
- Log messages differ from the C decoder's; return codes and frames do not.
- `cram/toon.avi` in the FATE suite is cut at 1 MiB: 116 of its 2822 frames decode, in both decoders.
