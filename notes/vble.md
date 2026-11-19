# VBLE (`vble.c`, 213 lines, plus the SSE2 median predictor in `x86/lossless_videodsp.asm`)

## Summary

- **FFmpeg tests:** 1 of 1 (`fate-vble`, the only one), with the port as the default decoder. `results/vble-fate.txt`
- **Differential fuzz:** 39,548,200 packets (4 processes, 900 s each: 3 on generated streams, 1 mutating the real sample), 0 mismatches, 0 panics. `results/vble-fuzz.txt`
- **Sanitizer fuzz:** 6,982,743 packets (4 processes, 300 s each), clean: no mismatches, no panics, no ASan or UBSan reports. That build has no x86 assembly, so it compares against the C median predictor, not the SSE2 one. `results/vble-fuzz-asan.txt`
- **Benchmark, Rust time / C time** (C with its assembly, one thread, 1280x720):

  | | straight port (v1) | final (v4) |
  |---|---|---|
  | the one real clip (4 frames) | 1.09 (1.03 in an earlier run) | 0.19 |
  | per path (random, smooth, flat) | 1.04 to 1.07 | 0.22 to 0.33 |

  There is only one clip, so the clip "range" is a single number. v1 to v3 were measured while nine other agents were running; the final table was measured at load average 1.9. `results/vble-bench-*.txt`
- **Versions:** 4 (straight port, then 3 optimisation steps). Parity was already there at v1 within noise; v2 was the big step.
- **Bugs in the port:** none found by any check. FATE passed on the first build and the first fuzz run was clean. (One false alarm was my own manual-check script, which hashed stderr together with stdout.)
- **C behaviour that had to be copied:** see "C behaviour copied" below. The main one: for odd widths the last chroma column is never decoded, and FFmpeg's SSE2 predictor overwrites it anyway with values computed from code lengths, while the C predictor leaves it alone. The port does whichever the FFmpeg build it is linked into would do.
- **Wrong in the C decoder:** (1) output for odd widths depends on CPU flags, reproduced; (2) odd sizes leave the last chroma column of the first row and the last chroma row unwritten; (3) the SSE2 predictor reads up to 15 bytes past the end of the decoder's values buffer, found by reading, **not confirmed with a tool**. Details and reproduction below.
- **Not ported / not covered:** frame buffers from a caller's own `get_buffer2` with strides that are not a multiple of 16; PPC and RISC-V predictors; frame contents other than zero in the samples C leaves unwritten (covered by a unit test only, not against C); packets above 4 MB. Details below.
- **Changes needed in shared files:** none.
- **Not verified:** the heap over-read (3); the explanation of the one mutant the fuzzer missed; frame threading beyond one manual run on the real sample; the claim that non-zero bytes after a packet cannot change the result (an argument, below, not a test).

## Files

| File | Lines |
|---|---|
| `rust/vble-core/src/lib.rs` | 319 of code without tests and comments (704 in all) |
| `rust/ffsafe-ffi/src/vble.rs` | 163 without tests |
| `shim/vble_rs.c` | 162 |
| `harness/codec_vble.c` | 238, of which about 110 are a VBLE encoder |
| `scripts/codecs/vble.sh` | 28 |

## The format

A packet is a 32-bit version (1) and a little-endian bitstream in two parts: one code length (0 to 8) per sample, written as that many zero bits and a one bit; then, for every sample with a non-zero length, that many bits. A code of length `n` with bits `b` is the number `v = 2^n - 1 + b`, and the residual is `(v >> 1) ^ -(v & 1)`, low 8 bits. Residuals are added to the left neighbour in the first row of a plane and to the median of left, top and left + top - top-left in the other rows; the first sample of every row is stored as it is.

The number of codes is `av_image_get_buffer_size(YUV420P, w, h, 1)`, which rounds the chroma planes up. The decoder then works on chroma planes rounded **down** (`width / 2`, `height / 2`). For odd sizes the codes left over are read, counted in the "enough bits left" check, and ignored.

## Versions

| Version | What changed | Clip | Paths | Measured |
|---|---|---|---|---|
| v1 straight port | `results/vble-lib-v1-straight-port.rs` | 1.09 | 1.04 to 1.07 | loaded machine |
| v2 length table | Code lengths read a byte at a time from a 256-entry table; residual bits read without a branch on length 0 | 0.40 | 0.47 to 0.61 | loaded |
| v3 wavefront | Median prediction of 15 rows at once, in plain Rust | 0.24 | 0.33 to 0.45 | loaded |
| v4 final | The same predictor with SSE2 intrinsics, portable twin kept; residual loop in its own function | 0.19 | 0.22 to 0.33 | load 1.9 |

**v2.** The C decoder's length loop peeks 8 bits, looks the length up, and moves on: one sample per iteration, each waiting for the position left by the one before. The lengths are simply the gaps between consecutive one bits, so the port takes a byte at a time, stores the up to eight lengths that end in it with one 8-byte store from a table, and carries the count of trailing zeros to the next byte. The last few lengths go through a loop that works like the C one. The sum of the lengths is not accumulated: every code takes its length plus one bit, so the sum is the end position minus the number of codes. I did not profile the C decoder; that its length loop is where most of its time goes is an estimate from the instruction sequence, supported by the size of the gain.

**v3.** FFmpeg's SSE2 predictor has 16-byte registers but still computes one sample per step, because each sample needs the one to its left. Rows can overlap instead: a row can start once the row above is two samples ahead. The port predicts a band of 15 rows together, each two samples behind the one above; one step computes one sample of every row. A sixteenth lane carries the row above the band (fed with its differences, it reproduces that row by summing them), so every lane gets its top and top-left samples by shifting the previous outputs up one lane, with no memory access in the loop. Residuals are decoded straight into this layout, so the only extra work is copying the result out. In plain Rust the compiler did not turn the 16-lane step into vector code in the real crate (it produced scalar code, with no vector min or max in it), although the same function in isolation was partly vectorised.

**v4.** The step written with `core::arch::x86_64` intrinsics: a loop of about 15 instructions for 16 samples. All intrinsics used take and return values, so the crate keeps `#![forbid(unsafe_code)]`; the single `unsafe` call of `restore_sse2` is in the FFI module behind `is_x86_feature_detected!("sse2")`. The portable version stays, a unit test in `ffsafe-ffi` checks that both agree at ten sizes, and the test hook `vble_rs_set_simd` lets the fuzzer run a quarter of its cases through the portable one. On a 1280x720 frame the SSE2 step saved about 0.45 ms of about 2.5 to 4 ms (measured with the ignored `timing` test in `ffsafe-ffi`, on a loaded machine).

I stopped at four. The remaining time is mostly the residual loop (about 15 instructions per sample).

## C behaviour copied

1. **Reads past the end of the packet.** The C bit reader (`get_bits.h`, checked, little-endian) lets the position run at most 8 bits past the end and reads the padding FFmpeg requires after a packet, which is zero. A length code that runs off the end therefore reads zeros and fails as a length above 8. The port reads zeros past the end. Whether the padding really is zero cannot change the result: a code is accepted only if its terminating one bit is inside the packet, the "enough bits left" check then fails for any position past the end, and both failures return `AVERROR_INVALIDDATA`. This is an argument from the code; the harness always pads with zeros.
2. **Enough bits left** is checked for all codes, including the ones that are never decoded (odd sizes, or chroma under `AV_CODEC_FLAG_GRAY`).
3. **Codes above 255.** Lengths give code numbers up to 510; an encoder needs only 0 to 255. The upper half decodes to the same 256 residuals again and is accepted.
4. **A version other than 1** is only a warning.
5. **Odd width, last chroma column.** The chroma planes of the frame are `(width + 1) / 2` wide but only `width / 2` columns are decoded. The first row of that column is never written. In the rows below, the SSE2 predictor stores whole blocks of 16 samples, so unless `width / 2` is a non-zero multiple of 16 it also writes the column after the row: a median prediction from the real left, top and top-left samples plus, as "residual", the byte that follows the row in the decoder's values buffer, which is the code length of the next row's first sample (or of the next plane's, or of the first left-over code). The C predictor writes nothing there. The port takes a flag from the shim, which asks the same question `ff_llviddsp_init` does (`ARCH_X86 && HAVE_X86ASM`, `EXTERNAL_SSE2(av_get_cpu_flags())`), and reproduces the SSE2 result as a pass over that one column.
6. **Width 1.** The chroma planes then have zero decoded columns, but the C code stores the first value of the first row without looking at the width. That "value" is the code length found at the plane's offset in the values buffer (the same offset for both chroma planes). With the SSE2 predictor every row below gets the same length.
7. **Odd height:** the last chroma row is never written. **Gray flag:** chroma is never written. The port leaves the same samples untouched.

## Wrong in the C decoder

1. **Output depends on CPU flags for odd widths** (item 5 above). Reproduce: `results/vble-manual-checks.sh` changes the width field of the FATE sample from 1280 to 1279 (the pictures become noise, but decode) and prints frame hashes: `ffmpeg -c:v vble` and `ffmpeg -cpuflags 0 -c:v vble` differ; at 1278 and 1249 (chroma width 624, a multiple of 16) they agree. The port matches the C decoder in each case. Output in `results/vble-manual-checks.txt`.
2. **Unwritten samples at odd sizes** (items 5 to 7). With FFmpeg's own frame pool they are zero; with a caller's buffers they are whatever was there, and with the SSE2 predictor the first of them feeds the column below it.
3. **Read past the end of a heap allocation, by reading, not confirmed.** `ff_add_median_pred_sse2` loads the residuals in blocks of 16. For the last row of the last chroma plane the block can extend past `ctx->val`, which is exactly as large as the code count: with both dimensions even and `width / 2` not a multiple of 16 the over-read is `16 - (width / 2) % 16` bytes. Example: 8x4, 48 bytes allocated, the last row of plane 2 starts at 44 and 16 bytes are loaded, 12 of them past the end. The bytes read only reach samples in the row padding, so the picture is not affected. To confirm: run the C decoder under Valgrind on such a file (for instance the 1278-wide file from the script above, 1 byte over). The container has no Valgrind, and AddressSanitizer does not see reads made by assembly, so I did not confirm it.

None of these has been reported upstream.

## Do the checks have teeth?

`results/vble-mutants.txt`: 8 broken variants. Unit tests caught 7, FATE 4, the 10-second differential fuzz 7.

- The unit tests missed the SSE2 predictor variant: `mutants.sh` runs the core crate's tests, and the SSE2-against-portable test lives in `ffsafe-ffi` because calling the SSE2 function needs `unsafe`.
- The fuzzer missed "zero-width plane gets no sample". Explanation, not confirmed by running it: the variant changes only the branch taken without the SSE2 overwrite, and in the build with assembly a zero-width plane always takes the other branch. The sanitizer build (no assembly) should catch it.

## Not ported, not covered

- **Caller-supplied frame buffers with unusual strides.** With a chroma stride smaller than the width rounded up to 16, the SSE2 predictor's block stores run into the next row; for the last decoded row of an odd-height picture that would be the visible, otherwise unwritten last chroma row. FFmpeg's allocator never produces such strides and the API asks callers to align them. The port does not reproduce this.
- **Other architectures.** `ff_llviddsp_init` also has PPC and RISC-V versions; the shim only models x86.
- **Non-zero frame contents.** The port reads the one unwritten sample the SSE2 predictor reads (first row, last chroma column), so it should match for any buffer contents, but the fuzzer only sees FFmpeg's zeroed pool buffers. A unit test covers it against my reading of the assembly, not against C. A `get_buffer2` hook in `harness/codec_vble.c` that fills frames with a pattern would close this; I did not write it.
- **Frame threading** is declared (each thread's context gets its own Rust decoder) and was checked once by hand: 4 frame threads on the real sample give the same hashes as 1 thread and as C.
- **Packets above 4 MB** (the harness limit) and the `INT_MAX / 8` packet size check.
- **Real material:** one sample, 4 decodable frames (the fifth packet is cut off and both decoders reject it). Everything else comes from the encoder in the harness: random, smooth and flat planes, random codes including the unused upper half, sizes from 1x1 with odd sizes and rows that fill the stride.
- Log messages differ slightly; return codes and frames do not.
