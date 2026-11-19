# ffmpeg-safe-poc

A proof of concept for replacing FFmpeg one piece at a time with memory-safe code. Fourteen of FFmpeg 8.1.3's decoders have been ported to safe Rust, plugged back into FFmpeg, shown to produce the same output as the originals, and tuned until they are as fast.

The first port (Apple RPZA) established the loop. Three more were done to see whether the loop repeats, then ten more, chosen to get harder: bit readers, Huffman tables, predictors and inverse DCTs that FFmpeg implements in x86 assembly, and two audio decoders.

| Decoder | C source | Why it was picked |
|---|---|---|
| `rpza` Apple RPZA | `rpza.c`, 195 lines | Small, has a real sample and an encoder. The pilot. |
| `msrle` Microsoft RLE | `msrle.c` + `msrledec.c`, 312 | Vulnerability on record (CVE-2013-2496). Palettes, three bit depths. |
| `cinepak` Cinepak | `cinepak.c`, 356 | Vulnerability on record (CVE-2014-8546). Keeps codebooks between frames. |
| `v210` v210 | `v210dec.c`, 179, plus assembly | FFmpeg unpacks it with hand-written SSSE3/AVX/AVX2/AVX-512 code. |
| `qtrle` QuickTime Animation | `qtrle.c`, 593 | Run-length coding at six bit depths. Still in use. |
| `msvideo1` Microsoft Video 1 | `msvideo1.c`, 351 | Block codec with state between frames. |
| `targa` Targa image | `targa.c`, 312 | Image format with a conformance suite. |
| `vble` VBLE | `vble.c`, 213, plus assembly | Small, but its median predictor is SSE2 assembly in FFmpeg. |
| `loco` LOCO | `loco.c`, 354 | Golomb-Rice bit reading. |
| `magicyuv` MagicYUV | `magicyuv.c`, 694, plus assembly | Huffman tables, three predictors in AVX2 assembly. |
| `aic` Apple Intermediate Codec | `aic.c`, 501, plus assembly | Inverse DCT. |
| `prores` Apple ProRes | `proresdec.c`, 891, plus DSP and assembly | Inverse DCT in assembly. Widely used. |
| `flac` FLAC | `flacdec.c`, 833, plus DSP and assembly | First audio decoder. LPC in assembly. Widely used. |
| `tta` TTA | `tta.c`, 441, plus assembly | Audio. Adaptive filter in assembly. |

## Result

| Decoder | FFmpeg's own tests | Fuzzed against C, packets | Same under sanitizers | Clips, Rust/C time | Single paths, Rust/C time |
|---|---|---|---|---|---|
| `rpza` | 5 of 5 | 262 million | 30.8 million | 0.43 to 0.87 | 0.11 to 0.97 |
| `msrle` | 5 of 5 | 178 million | 31.2 million | 0.58 to 0.73 | 0.11 to 1.02 |
| `cinepak` | 8 of 8 | 109 million | 9.7 million | 0.85 to 1.04 | 0.48 to 1.06 |
| `v210` | 9 of 9 | 187 million | 27.7 million | 0.87 to 1.03 | 0.98 |
| `qtrle` | 17 of 17 | 109 million | 15.2 million | 0.28 to 1.05 | 0.12 to 1.05 |
| `msvideo1` | 6 of 6 | 113 million | 14.9 million | 0.42 to 0.64 | 0.15 to 0.63 |
| `targa` | 12 of 12 | 136 million | 20.6 million | 0.10 to 1.05 | 0.04 to 1.00 |
| `vble` | 1 of 1 | 17.4 million | 3.4 million | 0.19 (one clip) | 0.22 to 0.33 |
| `loco` | 2 of 2 | 18.5 million | 2.7 million | 0.91 to 0.94 | 0.63 to 0.95 |
| `magicyuv` | 11 of 11 | 13.9 million | 1.9 million | 0.77 to 0.88 | 0.72 to 1.61 |
| `aic` | 2 of 2 | 8.9 million | 1.5 million | 0.81 to 0.86 | 0.36 to 0.83 |
| `prores` | 31 of 31 | 23.4 million | 2.6 million | 0.56 to 0.93 | 0.43 to 0.93 |
| `flac` | 36 of 36 | 151 million | 17.8 million | 0.42 to 0.92 | 0.52 to 0.88 |
| `tta` | 8 of 8 | 27.0 million | 1.1 million | 0.40 to 0.83 | 0.40 to 1.05 |

Every fuzz and sanitizer run in the table ended with no mismatch and no panic. The C side always has FFmpeg's x86 assembly enabled (except in the sanitizer build, which has none), and both decoders run with one thread. Run-to-run variation is up to about 10%, so ratios between 0.9 and 1.1 are parity, not a win or a loss.

One row is not at parity: MagicYUV's raw (uncoded) 8-bit slices take 1.61 times as long as FFmpeg's AVX2 assembly, and its raw 10-bit slices 1.11 times. Every MagicYUV clip is faster than C, because real streams are mostly Huffman-coded.

Packet counts differ by two orders of magnitude because packets do: a FLAC or RPZA case is a few hundred bytes, a ProRes or AIC case is a whole picture of DCT blocks.

All numbers are in `results/`, one set of files per decoder, and each decoder of the second and third batch has a write-up in `notes/`.

## How it is wired

- `rust/NAME-core`: one crate per decoder. `#![forbid(unsafe_code)]`, no dependencies. All bitstream parsing and every sample write happen here.
- `rust/ffsafe-ffi`: the C entry points for all of them. The only `unsafe` in the project: it turns (pointer, length) pairs into slices, owns decoder objects, makes the one call into each SIMD function, and catches panics.
- `shim/NAME_rs.c`: FFmpeg glue. Registers the decoder as `NAME_rs`, gets the frame from FFmpeg, and hands Rust the packet and the visible samples. It never reads packet bytes or writes samples.
- `scripts/build.sh` copies the FFmpeg source, adds the shims and two registration lines per decoder, and builds. Each `NAME_rs` is listed before the C decoder, so FFmpeg uses it by default; the original stays available as `-c:v NAME` (or `-c:a NAME`).

The ports are only given the visible samples. Where the C decoder writes outside them (into row padding, or into rows the allocator adds below the picture), the port drops those bytes.

## The loop

1. Port the decoder as directly as possible (`results/NAME-lib-v1-straight-port`).
2. Run FFmpeg's tests with the port as the default decoder (`scripts/fate.sh`).
3. Fuzz it against the original (`harness/difffuzz.c`): the same packet sequences go to both decoders through the libavcodec API, and after every packet the return codes and the frames must match (format, flags, colour properties, every visible sample of every plane, the palette; for audio the sample format, count, rate, channel layout and every sample). Inputs are generated streams, random bytes, and mutations of real packets.
4. Benchmark both (`harness/bench.c`), per decoder path and on whole clips.
5. Where Rust is slower, change the Rust and go back to step 3.

`PORTING.md` is the full procedure, including how to work on several decoders at once in separate build trees.

## Was it repeatable?

| Decoder | FFmpeg's tests on the first build | Straight port, clips, Rust/C | Versions | Final, clips, Rust/C |
|---|---|---|---|---|
| `rpza` | pass | 0.90 to 1.31 | 4 | 0.43 to 0.87 |
| `msrle` | pass | 0.72 to 0.85 | 3 | 0.58 to 0.73 |
| `cinepak` | pass | 1.99 to 4.68 | 5 | 0.85 to 1.04 |
| `v210` | pass | 1.96 to 2.20 | 2 | 0.87 to 1.03 |
| `qtrle` | pass | 1.73 to 21.5 | 4 | 0.28 to 1.05 |
| `msvideo1` | pass | 0.71 to 0.98 | 4 | 0.42 to 0.64 |
| `targa` | pass | 0.98 to 3.33 | 2 | 0.10 to 1.05 |
| `vble` | pass | 1.09 | 4 | 0.19 |
| `loco` | pass | 1.24 to 1.29 | 6 | 0.91 to 0.94 |
| `magicyuv` | pass | 0.90 to 1.33 | 5 | 0.77 to 0.88 |
| `aic` | pass | 1.02 to 1.05 | 2 | 0.81 to 0.86 |
| `prores` | pass | 0.74 to 3.02 | 2 | 0.56 to 0.93 |
| `flac` | 13 of 36 | 1.13 to 3.39 | 3 | 0.42 to 0.92 |
| `tta` | pass | 1.03 to 1.46 | 4 | 0.40 to 0.83 |

The straight-port numbers of the last ten were measured while other builds were running on the machine; the final numbers of all fourteen were measured on an idle one.

Correctness came quickly: thirteen of the fourteen passed FFmpeg's tests on their first build (FLAC, the largest, failed 23 of 36 on its first and passes all 36 now). Speed did not come for free: a straight port was slower than C on at least some inputs in nearly every decoder, and by 2 to 20 times in six. The fixes were of the same kind each time: keep a slice of the current row instead of recomputing offsets, write fixed-size chunks so bounds checks disappear, build pixels in 32- or 64-bit words, read bits through a 64-bit window, and, where FFmpeg has assembly, use SIMD.

The ten decoders of the last batch were each ported by a separate AI agent following `PORTING.md`, then checked again independently: FFmpeg's tests, the differential fuzz, the sanitizer fuzz, the broken variants and the benchmark were all re-run from the main build tree, and the numbers in this file are from those runs. That second pass mattered (see TTA below).

### Safe code against hand-written assembly

Seven of the fourteen decoders have x86 assembly in FFmpeg for their inner loop. The ports of all seven use SIMD too, through Rust's intrinsics: the ones needed take and return values, not pointers, and Rust lets safe code call those inside a function compiled for the instruction set. So every decoder crate still forbids `unsafe`. The one thing safe code cannot do is promise that the CPU has AVX2; that single `unsafe` call per decoder is in the FFI crate, behind a CPU check. Each has a portable twin, a test that the two agree, and a switch so the fuzzer compares both against C.

- ProRes: the inverse DCT is the first real DSP kernel in the project. FFmpeg's is SSE2/AVX assembly on 128-bit registers; the port's is AVX2 on 256-bit registers and the decoder is 0.56 to 0.93 of FFmpeg's time. Matching it bit for bit meant reproducing where the assembly and FFmpeg's own C version disagree (rounding in 16 bits, saturation instead of truncation), because FFmpeg picks between them by CPU.
- MagicYUV: the predictors match the AVX2 assembly on coded streams; raw 8-bit slices are the one row in the project that stayed slower (1.61). The worker's explanation, which has not been checked independently, is that the compiler emits several instructions for a vector load the assembly does in one.
- This is still not H.264, HEVC or AV1. Those are an order of magnitude more code and assembly than anything here.

## What the C decoders do that the ports had to copy

Differential testing against the original means matching it byte for byte, including behaviour that depends on FFmpeg's frame layout or build rather than on the bitstream. A few examples; each decoder's notes list its own:

- Microsoft RLE and QuickTime Animation bound runs by the frame's stride, not the picture width, so a run may continue through a row's padding into the next row.
- Cinepak blocks can stick out past the end of a row and land on the next one when the stride equals the row length.
- Bit readers that run past the end of a packet read FFmpeg's zero padding, and each decoder reacts differently: VBLE fails, AIC carries on, MagicYUV decodes up to seven bytes of the next slice.
- AIC and ProRes produce different pictures from damaged input depending on whether FFmpeg was built with assembly and which CPU flags are on. The shim asks the same question FFmpeg does and the port reproduces whichever applies.
- Most of them output a picture even when the packet is damaged.

## Findings in FFmpeg's decoders

None of these has been reported upstream. The first two were reproduced by hand; the others are in the notes with the worker's reproduction, which has not been repeated independently unless stated.

- **v210**: for pictures less than four rows tall the decoder returns the frame without decoding it (its slice count is `av_clip(thread_count, 1, height / 4)`, which is 0). A 48x2 file decodes to all zeros.
- **Targa**: when the data ends inside the pixel of a run packet, the decoder repeats bytes of an uninitialised stack array. `results/targa-uninit-repro.tga` (24 bytes) decodes to `11 bb cc` where `bb cc` are left over from the previous run; the port gives `11 00 00`.
- **LOCO**: signed integer overflow (undefined behaviour) at `loco.c:75`, reachable with about a megabyte of zero bytes on a three-pixel picture; UBSan reports it (`results/loco-ub-sanitizer.txt`).
- **AIC**: coefficients leak from one slice or frame into the next at some picture widths (`results/aic-stale-coeffs-repro.c`).
- **VBLE**: for odd widths the output depends on CPU flags, because the assembly predictor stores whole blocks of 16 over a column the C predictor leaves alone (`results/vble-manual-checks.sh`).
- **FLAC**: the worker recorded the C decoder crashing with a segmentation fault when the caller presets a floating-point sample format (`results/flac-c-null-call.txt`). Not yet written up or checked.
- MagicYUV, QuickTime Animation and Microsoft Video 1 have smaller oddities, described in their notes.

For each of these the fuzzer is kept away from exactly that input and nothing more, with a switch to turn the avoidance off.

## What went wrong on our side

- **The fuzzer was not comparing palettes.** A plane-count test returned before the palette case, for every decoder, from the start. It was found when a deliberately broken palette variant slipped through. After the fix, Microsoft RLE (111 million packets) and Cinepak (70 million) were fuzzed again with no mismatch, and the palette decoders of the last batch were verified with the fix in.
- **TTA was reported clean and was not.** Its worker's fuzz (2.6 million packets) found nothing; the independent run found two mismatches after about 10 million. The cause was one comparison in the port's 64-bit bit window (`<=` for `<`), which let the window fill completely and a later shift by 64 wrap silently in a release build. Fixed, a unit test and a broken variant added, then 27 million packets clean.
- **That bug prompted a new check**, `scripts/overflow.sh`: build the Rust with overflow checks and debug assertions on, so such an operation panics, and fuzz against that. All fourteen ports were run through it. It found one more problem, at the `unsafe` boundary, in C: the MagicYUV shim applied FFmpeg's `AV_CEIL_RSHIFT` macro to an unsigned value, which gave the chroma planes of every 4:2:0 and 4:2:2 picture a length near 2^63, and the FFI module built slices of that length. The decoder never wrote outside the real planes (the sanitizer runs were clean), but with a slice like that no bounds check behind it means anything, so for those planes the port was no safer than the C. Fixed in the shim, refused in the FFI module, and the other shims were checked for the same mistake. The lesson is about where the risk in this design sits: in the hundred lines of glue, not in the decoder.
- Smaller ones: a v210 row ending in five pixels lost its last chroma sample (caught by a unit test; FFmpeg's tests passed with the bug in); a Cinepak palette index error (caught reading the code back); a shell mistake that made one column of the broken-variant table always say "missed".

## Do the checks have teeth?

`scripts/mutants.sh NAME` builds deliberately broken variants of a decoder and records which check notices each one. See `results/NAME-mutants.txt`.

| Decoder | Broken variants | Caught by Rust unit tests | Caught by FFmpeg's tests | Caught by 10 s of differential fuzz |
|---|---|---|---|---|
| `rpza` | 5 | 0 | 4 | 5 |
| `msrle` | 5 | 4 | 1 | 5 |
| `cinepak` | 5 | 0 | 4 | 5 |
| `v210` | 6 | 2 | 2 | 6 |
| `qtrle` | 8 | 7 | 4 | 8 |
| `msvideo1` | 8 | 8 | 3 | 8 |
| `targa` | 8 | 7 | 1 | 8 |
| `vble` | 8 | 7 | 4 | 7 |
| `loco` | 6 | 4 | 1 | 6 |
| `magicyuv` | 6 | 3 | 1 | 5 |
| `aic` | 9 | 2 | 2 | 9 |
| `prores` | 6 | 2 | 2 | 6 |
| `flac` | 6 | 4 | 2 | 6 |
| `tta` | 7 | 4 | 2 | 7 |

The differential fuzzer caught 91 of 93. FFmpeg's tests caught 33: they miss what their samples do not contain. The two the fuzzer missed, and a sixth Cinepak variant not counted above, are explained in the notes as code that behaves identically (for MagicYUV and Cinepak) or that only the build without assembly can reach (VBLE); the VBLE and MagicYUV explanations have not been confirmed independently.

Ten seconds is the gate, not the proof: the TTA bug above needed millions of packets, and after its generator was aimed at the end of the packet the same bug is found in about 2,500 cases.

## What this does not show

- H.264, HEVC, AV1 and the other large codecs are untested. The hardest thing here is ProRes.
- The ports are single-threaded. FFmpeg's ProRes, MagicYUV and v210 decoders can split a frame across threads; the ports do not, and all benchmarks use one thread for both sides.
- None of these decoders has a known live vulnerability. The two CVEs were fixed years ago. This demonstrates the method, not a bug fix.
- For the decoders without assembly, the speed-up comes from restructuring, not from the language. The same changes would speed up the C.
- Several decoders have no encoder in FFmpeg (VBLE, LOCO, AIC, the low bit depths of QuickTime Animation), so their benchmark clips are a handful of real samples plus streams written by small encoders in the harness.
- Fuzz volume is uneven. FLAC, Targa and the run-length decoders have over 100 million packets; AIC has 9 million and VBLE 17 million. The TTA bug shows that a rare bit-reader state can hide below 10 million.
- Caller-supplied frame buffers (custom `get_buffer2`, unusual strides, negative line sizes) are not fuzzed; several shims refuse negative line sizes that the C decoder would accept.
- Microsoft RLE's 16-bit and 32-bit modes, AIC's alternative IDCT options, and non-x86 builds are not ported or not tested.
- FLAC has no write-up in `notes/` yet, and the ProRes notes were completed from the verification results after its worker was interrupted.
- Benchmarks ran in Docker on WSL2, pinned to one core of an i7-14700, C built by gcc 12 at FFmpeg's default `-O3` with its x86 assembly enabled, Rust 1.100.
- Log messages differ slightly from the originals; return codes and frames do not.

## Reproduce

Needs Docker. Nothing is installed on the host. The FFmpeg source is not in this repository; fetch it first.

```bash
git clone --depth 1 --branch n8.1.3 https://github.com/FFmpeg/FFmpeg.git ffmpeg
docker build -t ffsafe-poc docker
docker run -d --name ffsafe -v "$PWD:/proj" -v ffsafe-build:/build ffsafe-poc sleep infinity
docker exec ffsafe bash /proj/scripts/build.sh              # Rust + FFmpeg with every port
docker exec ffsafe bash /proj/scripts/fate.sh               # FFmpeg's tests for all of them
docker exec ffsafe bash /proj/scripts/harness.sh            # harnesses + test clips
docker exec ffsafe bash /proj/scripts/verify.sh prores      # tests, fuzz, sanitizers, broken variants
docker exec ffsafe bash /proj/scripts/bench.sh prores       # benchmark table for one decoder
docker exec ffsafe bash /proj/scripts/overflow.sh prores    # fuzz with overflow checks on
```

`ffmpeg/` is an unmodified shallow clone of FFmpeg tag `n8.1.3`; the build works on a copy inside the container.

## License

LGPL 2.1 or later, the same as the FFmpeg files these ports are derived from (see `LICENSE`). Each Rust decoder is a port of the FFmpeg source file named at the top of its `lib.rs`; the copyright of the original authors, listed in those FFmpeg files, continues to apply. This project is not affiliated with or endorsed by FFmpeg.
