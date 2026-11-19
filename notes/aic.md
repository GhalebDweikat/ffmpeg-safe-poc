# aic (Apple Intermediate Codec)

Port of `libavcodec/aic.c` (501 lines) plus the parts of FFmpeg it calls: the bounds-checked bit reader, unary and Exp-Golomb codes, the 8-bit "simple" IDCT (C and x86-64 assembly forms) and `put_signed_pixels_clamped`.

## Summary

- **FFmpeg tests:** 2 of 2 pass with the port as the default decoder (`fate-aic`, `fate-aic-oddsize`; `results/aic-fate.txt`). They passed on the first build.
- **Differential fuzz:** 10,026,959 packets (5 processes, 600 s each), 0 mismatches, 0 panics (`results/aic-fuzz.txt`). The first fuzz of the straight port was also clean.
- **Sanitizer fuzz: NOT RUN.** The coordinator took it over because the script's configure step can hang. There is no `results/aic-fuzz-asan.txt`.
- **Benchmark, Rust/C decode time** (measured while other agents were using the machine):

  | | clips (2 real samples) | per path (6 generated) |
  |---|---|---|
  | v1 straight port (11 passes) | 1.02 to 1.05 | 0.99 to 1.28 |
  | v2 final (31 passes) | 0.82 to 0.86 | 0.36 to 0.81 |

- **Versions:** 2 (straight port, final).
- **Bugs in the port:** none reached a check: unit tests, FFmpeg's tests and the first fuzz were clean. Two mistakes outside the decoder: the packet generator divided by zero (the four generated-mode fuzz processes died with SIGFPE on the first gate run), and the first SSE2 version did not compile (see "Safe SIMD").
- **C behaviour copied:** stale coefficients kept between slices and frames; reading past the end of a slice; two IDCTs with different overflow behaviour; the last macroblock row drawn against the bottom edge over the row above; writes outside the visible picture dropped. Details below.
- **Wrong in the C decoder:** (1) coefficients leak from one slice or frame into the next for some picture widths; (2) the picture decoded from out-of-range coefficients depends on the build and on CPU flags. Both have a reproducer in `results/`.
- **Not ported:** IDCTs other than the 8-bit simple one (`-idct int`, `faani`, `xvid`, `bits_per_raw_sample` 9, 10 or 12): the port refuses to open, the C decoder decodes.
- **Changes needed in shared files:** none.
- **Not verified:** the sanitizer run; anything on a CPU architecture other than x86-64; frame threading beyond two real clips; benchmark numbers on an idle machine.

## Files

| File | Lines |
|---|---|
| `rust/aic-core/src/lib.rs` | 1385 (820 of code before the tests, about 100 of them tables) |
| `rust/ffsafe-ffi/src/aic.rs` | 206 |
| `shim/aic_rs.c` | 207 |
| `harness/codec_aic.c` | 365 |
| `scripts/codecs/aic.sh` | 28 |

## Which C functions the decoder gets on this machine

`ff_idctdsp_init` gives the C decoder, on x86-64 with assembly and default options (and with `-idct simple`, which FFmpeg's tests use): `ff_simple_idct8_avx` (`x86/simple_idct10.asm`, transposed coefficient order) and `ff_put_signed_pixels_clamped_sse2`. Without assembly (`--disable-x86asm`, as in the sanitizer build) or with CPU flags forced to 0 it gets `ff_simple_idct_int16_8bit` and `put_signed_pixels_clamped_c`.

The two pixel stores give the same result for every input. The two IDCTs do not (see below). The shim (`which_idct` in `shim/aic_rs.c`) repeats FFmpeg's choice and tells Rust which form to reproduce, so the port matches the C decoder in both builds.

## Versions

1. **Straight port** (`results/aic-lib-v1-straight-port.rs`, `results/aic-bench-v1.txt`). Same structure as the C: four bands in one buffer, `recombine_block`, scalar IDCT. Clips 1.02 and 1.05, paths 0.99 to 1.28.
2. **Final** (`results/aic-lib-v2-final.rs`, `results/aic-bench-v2.txt`, `results/aic-bench-final.txt`). Three changes at once, not measured separately:
   - Coefficients are stored where the IDCT wants them as they are decoded: six blocks of 64 per macroblock, each transposed. The scan tables are folded into placement tables at compile time, and `recombine_block` disappears.
   - One code (unary or Exp-Golomb, plus extra bits; at most 38 bits) is read from one 64-bit word while eight bytes of the slice remain. The C reads a unary code bit by bit and a code with extra bits in two steps. Near the end of a slice the port falls back to reading exactly as C does.
   - Dequantisation, IDCT and clamping in SSE2 intrinsics, following the assembly: eight 1-D transforms at a time with `pmaddwd`. Pixels leave as eight 64-bit words per block.

Final table (`results/aic-bench-final.txt`):

| input | C us/frame | Rust us/frame | Rust/C |
|---|---|---|---|
| path:sparse | 3719.51 | 2272.41 | 0.611 |
| path:sparse-il | 3619.48 | 2282.89 | 0.631 |
| path:sparse-unary | 4470.45 | 2271.38 | 0.508 |
| path:dense | 8920.03 | 7231.72 | 0.811 |
| path:dense-unary | 18638.04 | 6703.60 | 0.360 |
| path:dc | 1878.69 | 1112.77 | 0.592 |
| real-oddsize (481x241, progressive) | 509.83 | 419.71 | 0.823 |
| real-small (1440x1080, interlaced) | 6898.09 | 5942.70 | 0.861 |

The large wins on the unary paths come from the bit reader, not the language: C's `get_unary` loops over single bits. The real samples use Exp-Golomb codes with 1 or 2 extra bits, mostly with zero runs (`results/aic-sample-modes.py` prints this), which is what `path:sparse` imitates.

### Safe SIMD

The core crate still has `#![forbid(unsafe_code)]`. The SSE2 functions are marked `#[target_feature(enable = "sse2")]`; Rust requires that even though SSE2 is part of the x86-64 baseline (the compiler says so explicitly: the feature being on in the build configuration does not remove the requirement). So the one `unsafe` call of `Decoder::decode_sse2` is in the FFI module, behind `cfg!(target_feature = "sse2")`. There is a portable decoder, a test that the two block functions agree on 300,000 random blocks for both IDCT forms (in the FFI crate, because calling the SSE2 function needs `unsafe`), and a hook (`aic_rs_set_simd`) through which a quarter of the fuzz cases run the portable code.

AVX2 was not needed and not tried.

## C behaviour that had to be copied

- **Stale coefficients.** See the first finding below. The port keeps the buffer between slices and frames and clears exactly the entries the C `memset` clears. Because the port stores coefficients in a different order, a narrow slice clears them one by one through the placement function, and when the interlace flag changes between frames the luma coefficients are moved to their new places (`clear_narrow`, `change_layout`; both only run for pictures that have narrow slices).
- **Reading past the end of a slice.** The only length check is "at least 5 bits left" at the start of each band. Within a band the reader runs on: its position stops 8 bits after the slice end and every later read returns the bits found there, which are the first bytes of the next slice, or FFmpeg's zero padding after the last one. Unary codes never fail, so a truncated slice still decodes. Rust is given the rest of the packet after the slice start and treats bytes past the packet as zero. The C reader reads at most 5 bytes past the end of the packet, inside the padding.
- **Exp-Golomb error value.** `get_ue_golomb` returns a negative error for more than 12 leading zeros; the caller stores it in an `unsigned`, shifts it and adds bits before testing for 0x10000 or more. The result is always an error (worked out by hand for all seven shift amounts), so the fast path of the port returns the error directly; the path used near the end of a slice does the arithmetic as C does.
- **Two IDCTs.** See the second finding below.
- **A row with one coefficient.** Both IDCTs replace a row that has only its first coefficient by that coefficient times 8 instead of multiplying by 16383/2048. Copied in both forms.
- **Last macroblock row.** When the picture has more than one macroblock row, the last is drawn at `height - 16` (chroma at `(height + 1) / 2 - 8`), over the bottom of the row above. With a single row it is drawn at 0 and runs below the picture.
- **Writes outside the picture.** The C decoder always writes whole 16x16 macroblocks: into the row padding right of the picture, and into rows below it when the height is under 16. The port drops those writes. With FFmpeg's own frame allocation they never touch a visible sample.
- **A failed slice.** The C decoder returns the error after writing part of the frame; libavcodec then discards the frame, so nothing of it is visible. The port does the same. What does survive a failed slice is the coefficient buffer, which the port also keeps.
- **Checks before the frame is allocated** (packet shorter than the slice table, version, header size, frame size, dimensions) are made in the same order, each returning `AVERROR_INVALIDDATA`.

## Wrong in the C decoder

### 1. Coefficients leak between slices and frames

`aic_decode_init` puts the four bands in one buffer at offsets `ctx->slice_width * {0, 64, 96, 288}`. `aic_decode_slice` clears `slice_width * 384` values from the start, where `slice_width` is the width of the current slice. For a slice narrower than `ctx->slice_width` that leaves all or part of bands 1 to 3 untouched, and a band coded with zero runs does not overwrite what it skips. The picture then contains coefficients of the previous slice or the previous frame.

Affected: every slice of a picture up to 16 pixels wide, and the last slice of each row when the width in macroblocks is above 32 and has no divisor giving slices of at most 32 (37, 41, 43, 47, 53, ... macroblocks; 848 pixels wide is such a size). 1280, 1440 and 1920 wide are not affected.

With frame threads each thread has its own buffer, so for affected sizes the C output would also depend on which thread decodes which frame. That was reasoned from the code, not run.

Reproduce: `results/aic-stale-coeffs-repro.c` (build line in the file). Output, `results/aic-stale-coeffs-repro.txt`: a 16x16 frame with no coefficients at all decodes to U = 128 on its own and to U = 148 after a frame with one chroma coefficient. The port gives the same two numbers.

### 2. The picture from out-of-range coefficients depends on the build

The decoder passes any 16-bit value to the IDCT. FFmpeg's C IDCT stores the row pass results in `int16_t`, keeping the low 16 bits. The x86-64 assembly packs them with saturation (`packssdw`), and adds the column rounding term to the first coefficient with a 16-bit `paddw`, which wraps above 32735. So the same packet gives different pictures with and without assembly, and with `-cpuflags 0`.

Reproduce: `results/aic-idct-overflow-repro.c`. Output, `results/aic-idct-overflow-repro.txt`: the Y plane hashes to `cdca6700767e56c6` with default CPU flags and to `3aa5417cf1174930` with flags off, for the C decoder and for the port alike.

No encoder writes such coefficients. It only matters for damaged or hostile input.

Neither finding has been reported upstream.

## Do the checks have teeth?

`results/aic-mutants.txt`: nine broken variants.

| | caught |
|---|---|
| differential fuzz, 10 s | 9 of 9, each within 27 cases |
| FFmpeg's tests | 2 (last row position, quantisation matrix entry) |
| unit tests of the core crate | 2 (buffer cleared for every slice, reader not stopping) |

FFmpeg's tests miss both IDCT overflow variants, the single-coefficient row, the longest Exp-Golomb code, the read past the slice end, the stale buffer and the interlace change. The three IDCT variants change only the SSE2 code, which the core crate's unit tests cannot call.

## The fuzzer's generator

FFmpeg has no AIC encoder. `harness/codec_aic.c` writes a valid header and slice table and slices of random coefficients: either code type, 0 to 7 extra bits, with and without zero runs, progressive and interlaced, values that are small, anywhere in 16 bits, or mostly extremes. Some slices are declared shorter or longer than they are. It does not transform a picture.

`config` adds widths of 529 to 1024 so that rows have several slices, including a narrower last one. `options` chooses the C decoder's IDCT (default; CPU flags 0; C IDCT with SSE2 store; `-idct simple`) and whether Rust uses SSE2. The two real samples are the seeds for the mutation mode.

Coverage was judged from the broken variants, not measured with a coverage tool.

## Not ported, not covered

- **Other IDCTs.** `-idct int`, `faani` and `xvid`, and `bits_per_raw_sample` of 9, 10 or 12, make FFmpeg pick a different IDCT. The port returns `AVERROR_PATCHWELCOME` from init; the C decoder decodes. Not fuzzed (it would show as an open mismatch).
- **Other architectures.** ARM, AArch64, PowerPC, RISC-V, MIPS, LoongArch and 32-bit x86 have assembly IDCTs of their own. The shim uses the C form there. Not compared, not built.
- **Frame threading.** The capability is declared as in the C decoder, since each thread gets its own decoder object. Checked only by decoding the two real clips with 1 and 4 threads: same frame checksums for both decoders. All fuzzing and benchmarks are single-threaded.
- **Frames from a caller's own `get_buffer2`** with rows narrower than the width rounded up to 16: the C decoder would write past the row. Not covered.
- **Width or height changed on the context after init.** The port clips and does not panic; it was not compared with C, which would write out of bounds.
- **Log messages** differ slightly; return codes do not.

## Not verified

- The sanitizer fuzz (`asan.sh`) was not run by me.
- Benchmarks were taken while other agents were running; v1 used 11 passes, the final table 31.
- The three changes of v2 were not measured one at a time.
