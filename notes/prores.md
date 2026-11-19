# prores: Apple ProRes

C source: `libavcodec/proresdec.c` (891 lines), `proresdsp.c`, `proresdata.c`, the inverse DCT in `simple_idct_template.c`, and the x86 assembly `x86/proresdsp.asm` with `x86/simple_idct10_template.asm`. The port registers as `prores_rs`.

## Summary

The worker that ported this decoder was interrupted before writing this section; it was written afterwards from the independent verification run (main build tree, idle machine).

- **FFmpeg's tests:** 31 of 31 pass with the port as the default decoder; they passed on the first build of the straight port.
- **Differential fuzz:** 23.4 million packets, no mismatch, no panic (`results/prores-fuzz.txt`). With overflow checks on: 2.8 million more, clean.
- **Sanitizer fuzz:** 2.6 million packets, clean (`results/prores-fuzz-asan.txt`).
- **Benchmark, Rust/C time, one thread, C with its assembly:** clips 0.56 to 0.93 (13 clips), single paths 0.43 to 0.93 (11 paths) (`results/prores-bench-final.txt`). The straight port was 0.74 to 3.02 on clips.
- **Versions:** 2 (straight port; AVX2 inverse DCT through safe intrinsics).
- **Broken variants:** 6. The differential fuzz caught 6, the unit tests 2, FFmpeg's tests 2 (`results/prores-mutants.txt`).
- **Not ported:** slice and frame threading, hardware acceleration.

## Files

| File | Lines | What |
|---|---|---|
| `rust/prores-core/src/lib.rs` | 756 | frame header, picture and slice tables, slices, alpha, the stores into the picture |
| `rust/prores-core/src/bits.rs` | | bit reader, codewords, DC and AC coefficients |
| `rust/prores-core/src/idct.rs` | | the three inverse DCTs, portable and AVX2 |
| `rust/ffsafe-ffi/src/prores.rs` | | C entry points, the one `unsafe` call of the AVX2 decoder, the test that AVX2 and portable transforms agree |
| `shim/prores_rs.c` | | FFmpeg glue |
| `harness/codec_prores.c` | | a small ProRes encoder for the fuzzer and the benchmark |

The core crate forbids `unsafe`, has no dependencies and does not allocate. The decoder keeps nothing between frames, so there is no decoder object: one call parses the frame header, the shim applies it to the codec context in the order the C decoder does (size change, alpha check, frame flags, pixel format, colour properties, truncation check, `skip_frame`, buffer), and a second call decodes into the visible planes.

## Which inverse DCT FFmpeg uses, and what the port does

`ff_proresdsp_init` picks, on this machine:

- 10-bit output (every tag except `ap4h` and `ap4x`): `ff_prores_idct_put_10_avx`, the assembly (same code as the SSE2 version, VEX-encoded, 128-bit registers), with the block stored transposed.
- 12-bit output (`ap4h`, `ap4x`): `prores_idct_put_12_c`. There is no assembly for 12 bits.
- 10-bit output without assembly (`--disable-x86asm`, which is what the sanitizer build uses, or `-cpuflags 0`): `prores_idct_put_10_c`.

The assembly and the C code for 10 bits are not the same function. They agree on ordinary coefficients and differ on extreme ones, in three places:

1. Row rounding. C adds `1 << 14` to a 32-bit sum. The assembly adds 1 to the first coefficient of the row in 16 bits (`paddw` with `pw_1`), which wraps from 32767 to -32768.
2. Row results. C stores `(a + b) >> 15` into an `int16_t` (keeps the low 16 bits). The assembly packs with `packssdw` (saturates).
3. Column rounding. C adds 8192 to the first row in 16 bits and then 8 in 32 bits. The assembly adds 8200 in 16 bits (`pw_88` is 0x2008).

So the port has three transforms (`Idct::Asm10`, `Idct::C10`, `Idct::C12`), each in portable Rust and in AVX2, and the shim chooses the way `ff_proresdsp_init_x86` does (build has assembly, CPU flags have SSE2 or AVX). All six are compared against FFmpeg by the fuzzer: a quarter of the fuzz cases force FFmpeg's CPU flags to 0 (both decoders then use the C transform), and a quarter turn the port's AVX2 off.

The 12-bit C code has a shortcut for rows with only a DC coefficient, `(dc + 1) >> 1`, which is not what its full transform gives (W4 is 32767, not 32768), so the shortcut is part of the output and is copied.

### The AVX2 transform

FFmpeg's assembly works on four 32-bit lanes at a time (`pmaddwd` on 128-bit registers, each pair of inputs done twice, low and high half), then transposes with 24 unpacks. The port:

- stores a block as four runs of 16 values, one run per input pair (columns 0 and 2, 4 and 6, 1 and 3, 5 and 7) for the eight rows, so one 32-byte load is one `pmaddwd` operand for all eight rows (the scan tables and quantisation matrices are permuted to match, as FFmpeg permutes them for its assembly);
- does each pass with 16 `vpmaddwd` on 256-bit registers;
- goes from the row results to the column inputs with 4 `vpshufb`, 8 unpacks and 4 `vperm2i128` instead of a transpose;
- for 12 bits, where three constants do not fit in 16 signed bits, multiplies by the constant minus 65536 and adds the input shifted left by 16 afterwards (all arithmetic is modulo 2^32 in the C code too), and selects the DC-only shortcut per row with a compare and blends.

Only value-in, value-out intrinsics are used, so the crate still forbids `unsafe`. Loads and stores go through arrays (`_mm256_set_epi64x` of four words built from the array, `_mm256_extract_epi64` back), which the compiler turns into plain vector loads and stores.

## Versions

| Version | What changed | Clips, Rust/C | Paths, Rust/C |
|---|---|---|---|
| v1 straight port (`results/prores-lib-v1-straight-port/`, `prores-bench-v1.txt`) | scalar transforms in natural block order | 0.74 to 3.02 (10-bit clips 1.21 to 3.02, 12-bit clips 0.74 to 0.96) | 0.84 to 3.39 |
| v2 (`results/prores-lib-v2-avx2-idct/`, `prores-bench-v2.txt`) | AVX2 transforms, blocks in load order, blocks stored row by row straight into the picture when fully visible, coefficient buffer kept across slices | 0.44 to 1.00 | 0.32 to 1.02 |

On an idle machine with 31 passes the final version measures 0.56 to 0.93 on clips and 0.43 to 0.93 on single paths (`results/prores-bench-final.txt`).

Both tables above were measured with 11 passes while other jobs were running.

Where the time goes: on the DC-only path the C decoder spends about 46 ns per block (assembly transform, clear, store), the straight port 158 ns. On the dense path entropy decoding dominates and was at parity from the first version (the Rust reader is a direct port of the `get_bits.h` macros: one unaligned 64-bit load per codeword). The 12-bit rows are far below 1 only because FFmpeg has no assembly there: scalar C against AVX2.

## C behaviour that had to be copied

- **Three inverse DCTs** that disagree on extreme coefficients (above).
- **Quantiser products in 16 bits.** `qmat[i] * qscale` is stored in an `int16_t`; with a matrix entry of 255 and the largest scale (512) it wraps. Coefficient times quantiser wraps in 16 bits as well.
- **Reads past the end of a plane.** The bit reader's position stops 8 bits after the end of the plane's data, and reads there see what follows in the packet: the next plane, the next slice, or FFmpeg's zero padding after the packet. DC decoding has no end check at all. The port reads the same bytes (the reader's slice runs to the end of the packet, and everything after reads as zero). The slice header is read to 8 bytes even when the slice is 6 bytes long, and the picture header to 8 bytes even when fewer are left.
- **The end-of-data test looks at a stale window.** `decode_ac_coeffs` stops when fewer than 32 bits are left and they are all zero, but it tests the 32-bit window that was loaded before the last level and already shifted by the level and sign bits. After a long level code the window has run dry and the test sees zeros that are not in the stream, so decoding stops early. Copied exactly (the reader keeps the same window).
- **A failed slice leaves the buffer as it was.** Nothing is written for a plane whose coefficients fail to decode, and the planes after it in the slice are skipped: luma failure leaves the whole slice untouched, a Cb failure leaves luma written, a Cr failure leaves luma and Cb written, alpha is never reached. The frame is still output (with `decode_error_flags` set) unless every slice of a picture failed. What shows through is whatever the buffer held: zeros in a new buffer, an earlier frame in a reused one. The port writes the same samples in the same order and gets its buffers at the same point, so it shows the same.
- **Return code -1 when every slice fails.** `decode_picture` returns `slices[0].ret`, which is the -1 each slice job starts with (the error paths return without storing their code). The caller sees -1 (`AVERROR(EPERM)`), not `AVERROR_INVALIDDATA`.
- **State changed before an error is found.** A header with a new size changes the context's dimensions even if the frame is then rejected (invalid alpha mode, truncated matrix, broken picture); the frame type of a rejected header is remembered; colour properties and pixel format are set before the truncation check.
- **Second field.** If data is left after the first picture of an interlaced frame, it is decoded as the second field; if not, the frame is output with one field. A picture size of 0 makes the second field decode the first picture's data again. A progressive frame ignores what follows its picture.
- **Header sizes are taken on trust.** A frame header size smaller than the header (even 0) just moves the start of the picture; a slice header size below 6 makes the plane data start inside the header.
- **Slice count** is computed from the picture size, the count in the header is ignored (as QuickTime does).
- **4:4:4 chroma block order** is column-wise (top, bottom, then the next column), luma is row-wise.
- **Alpha.** Reads continue at the stuck position after the data ends; a run is cut at the slice end; 8-bit alpha is widened by bit replication, 16-bit by a shift.

Writes outside the visible picture (the C decoder always writes whole macroblocks) are dropped. Unlike Cinepak and Microsoft RLE they have no effect on visible samples: a macroblock row never exceeds the stride, and the buffer pool is keyed by the picture size, so padding never becomes visible later.

## Things that are wrong in the C decoder

1. **The output depends on the build and on CPU flags.** `results/prores-idct-differs.mov` holds one 113-byte frame (16x16, quantisation matrix of ones, DC coefficient 32767). `ffmpeg -threads 1 -c:v prores -i results/prores-idct-differs.mov -f rawvideo -` gives luma 4 (black); with `-cpuflags 0` in front it gives 1019 (white). Cause: the 16-bit rounding add in the assembly (point 1 above). Seen first by reading the assembly, confirmed with FFmpeg as described; the port gives 4 and 1019 too.
2. **Damaged frames show stale memory, and not always the same.** `results/prores-damaged.mov` is a 160x120 clip with noise added by `-bsf:v noise=amount=400`; 51 slices fail. `ffmpeg -c:v prores -i results/prores-damaged.mov -f framecrc -` gives different checksums with `-threads 1` and with the default thread count, and with `-threads 1` it gave a different result in 1 run of 5 (the port likewise, in 1 or 2 runs of 5): which buffer a frame gets depends on when the rest of the program releases the previous ones. The decoder flags such frames only through `decode_error_flags`.
3. **-1 instead of an error code** when all slices fail (above).
4. **The stale-window end test** (above) can drop valid trailing coefficients. No encoder I know of produces such a stream; it is reachable with crafted input only.

None of these is a memory-safety problem. The sanitizer run on the C code (2.6 million packets, AddressSanitizer and UBSan) reported nothing.

## Bugs in the port, and what caught them

No check found a bug in the decoder itself: FFmpeg's tests passed on the first build, the first fuzz gate was clean, and the AVX2 transforms matched the portable ones on the first run of their test (600,000 blocks). What did go wrong:

- Four expectations in my own unit tests were wrong (the DC gain of the transform, a bit position, and an "extreme" block that both transforms happened to clip to the same value). Caught by running the tests; the decoder was right.
- The clip generator: FFmpeg's `prores` encoder refuses odd widths and `prores_ks` calls its top profile `4444xq`, not `xq`. Caught by `harness.sh` printing the encoder errors.

## What is not ported or not covered

- Slice threading and frame threading. The port declares neither; all comparisons are single-threaded. With several threads the C decoder is correspondingly faster on a multi-core machine; that was not measured.
- Hardware acceleration (VideoToolbox, Vulkan), not built here.
- `skip_frame` is mirrored in the shim but not exercised by any check.
- The fuzzer limits pictures to 2^21 pixels (`max_pixels`, set on both decoders), because a mutated header can ask for 65535 x 65535 and both decoders would allocate and clear such a frame before rejecting the packet. Larger pictures are covered only by the 1920x1080 clips and samples.
- Callers that supply their own buffers (`get_buffer2`) see their own buffer contents where slices fail, and for them writes into the padding (which the port drops) could matter if they reuse a buffer for a different picture size. Not tested.
- The sanitizer build has no assembly, so there the 10-bit comparison is against FFmpeg's C transform only.
- The generated fuzz streams use picture sizes up to 250 pixels a side; mutated real packets come from clips of 33x17 to 160x120 and one 1920x1080 sample.
- Big-endian hosts: the port assumes native 16-bit samples are little-endian in the same way FFmpeg's `AV_PIX_FMT_YUV422P10` is native; not tested.

## Changes needed in shared files

None. One remark: `fuzz.sh` and `iterate.sh` start one process per entry of `FUZZ_SEEDS` on top of the generated ones, so I joined eight small encodes into two seed files (`join` in `scripts/codecs/prores.sh`) to stay within six processes.

## Not verified

- The two reproductions under "Things that are wrong in the C decoder" were made by the porting worker and have not been repeated independently.
- Decoding with more than one thread.
- `skip_frame`, caller-supplied buffers, big-endian hosts (see above).
