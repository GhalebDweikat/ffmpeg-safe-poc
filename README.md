# ffmpeg-safe-poc

A proof of concept for replacing FFmpeg one piece at a time with memory-safe code. Four of FFmpeg 8.1.3's decoders have been ported to safe Rust, plugged back into FFmpeg, proven to produce the same output as the originals, and tuned until they are as fast.

The first port (Apple RPZA) established the loop. The other three were done to see whether the loop repeats, and each was picked because it adds something RPZA did not have:

| Decoder | C source | Why it was picked |
|---|---|---|
| Apple RPZA | `rpza.c`, 195 lines | Small, has a real sample and an encoder. The pilot. |
| Microsoft RLE | `msrle.c` + `msrledec.c`, 312 lines | Has a vulnerability on record (CVE-2013-2496, out-of-bounds access). Palettes, three bit depths. |
| Cinepak | `cinepak.c`, 356 lines | Has a vulnerability on record (CVE-2014-8546). Keeps codebooks from frame to frame. Four real samples in FFmpeg's test suite. |
| v210 | `v210dec.c`, 179 lines, plus 127 lines of x86 assembly | FFmpeg unpacks it with hand-written SSSE3/AVX/AVX2/AVX-512 code. |

## Result

| | RPZA | Microsoft RLE | Cinepak | v210 |
|---|---|---|---|---|
| FFmpeg's own tests, port as the default decoder | 5 of 5 | 5 of 5 | 8 of 8 | 9 of 9 |
| Differential fuzz against the C decoder, packets | 262 million | 178 million | 109 million | 187 million |
| Mismatches, panics | 0, 0 | 0, 0 | 0, 0 | 0, 0 |
| Same fuzz with AddressSanitizer and UBSan on the C code | 30.8 million, clean | 31.2 million, clean | 9.7 million, clean | 27.7 million, clean |
| Decode time on whole clips, Rust relative to C | 0.43× to 0.87× | 0.58× to 0.73× | 0.85× to 1.04× | 0.87× to 1.03× |
| Decode time per decoder path | 0.11× to 0.97× | 0.11× to 1.02× | 0.48× to 1.06× | 0.98× |
| Rust decoder, lines of code (C original) | 383 (195) | 317 (312) | 434 (356) | 260 (179 + 127 asm) |

Run-to-run variation is up to about 10%, so ratios between 0.9 and 1.1 are parity, not a win or a loss. For v210 the C side is FFmpeg's AVX2 assembly.

All numbers are in `results/`, one set of files per decoder. RPZA's 262 million packets were run on the first version of the fuzzer; after the fuzzer was generalised for the other decoders, RPZA was run again for 64 million more, also clean.

## How it is wired

- `rust/NAME-core`: one crate per decoder. `#![forbid(unsafe_code)]`, no dependencies. All bitstream parsing and every pixel write happen here.
- `rust/ffsafe-ffi`: the C entry points for all four. The only `unsafe` in the project: it turns (pointer, length) pairs into slices, owns the Cinepak decoder's heap allocation, and catches panics.
- `shim/NAME_rs.c`: FFmpeg glue, about 105 lines each. Registers the decoder as `NAME_rs`, gets the frame from FFmpeg, and hands Rust the packet and the visible image. It never reads packet bytes or writes pixels.
- `scripts/build.sh` copies the FFmpeg source, adds the shims and two registration lines per decoder, and builds. Each `NAME_rs` is listed before the C decoder, so FFmpeg uses it by default; the original stays available as `-c:v NAME`.

The ports are only given the visible image. Where the C decoder writes outside it (into row padding, or into rows the allocator adds below the picture), the port drops those bytes.

## The loop

1. Port the decoder as directly as possible (`results/NAME-lib-v1-straight-port.rs`).
2. Run FFmpeg's tests with the port as the default decoder (`scripts/fate.sh`).
3. Fuzz it against the original (`harness/difffuzz.c`): the same packet sequences go to both decoders through the libavcodec API, and after every packet the return codes and the whole frame must match (format, flags, every visible sample of every plane, the palette). Inputs are generated streams, random bytes, and mutations of real packets, at many frame sizes.
4. Benchmark both (`harness/bench.c`), per decoder path and on whole clips.
5. Where Rust is slower, change the Rust and go back to step 3.

What a new decoder needs, beyond the Rust itself: an FFI module (60 to 95 lines), a shim (about 105), a packet generator for the fuzzer and benchmark (`harness/codec_NAME.c`, 80 to 210), and a description file listing its FFmpeg tests, clips and broken variants (`scripts/codecs/NAME.sh`, about 25). The fuzzer, benchmark and all scripts are shared.

## Was it repeatable?

| | RPZA | Microsoft RLE | Cinepak | v210 |
|---|---|---|---|---|
| FFmpeg's tests on the first build | pass | pass | pass | pass |
| First differential fuzz | clean | clean | clean | clean |
| Straight port, clips, Rust/C time | 0.90× to 1.31× | 0.72× to 0.85× | 1.99× to 4.68× | 1.96× to 2.20× |
| Versions until it matched C | 4 | 3 | 5 | 2 |
| Final, clips, Rust/C time | 0.43× to 0.87× | 0.58× to 0.73× | 0.85× to 1.04× | 0.87× to 1.03× |

Correctness came quickly every time. Speed did not come for free: a straight port was slower than C on at least some paths in all four, and by 2× to 5× in two. The fixes were of the same kind each time: keep a slice of the current row instead of recomputing offsets, write fixed-size chunks so bounds checks disappear, and build pixels in 32- or 64-bit words so the compiler emits one store instead of several.

Bugs in the ports, and what caught them:

- Cinepak: palette blocks read the wrong codebook bytes. Caught reading the code back against the C, before the first build.
- v210: a row ending in five pixels lost its last chroma sample. Caught by a Rust unit test before the first fuzz run. FFmpeg's tests passed with the bug in: it needs a width that leaves five pixels over after the last group of six, and none of theirs does.
- The harness itself: the "unit tests" column of the broken-variant table was always "missed" because of a shell pipeline mistake. Fixed, and the RPZA table was run again.

### v210: safe code against hand-written assembly

The straight port of v210 took 2.1 times as long as FFmpeg's AVX2 assembly. The final version uses AVX2 too, through Rust's intrinsics: the ones it needs take and return values, not pointers, and Rust lets safe code call those inside a function compiled for AVX2. So the decoder crate still forbids `unsafe`. The one thing safe code cannot do is promise that the CPU has AVX2; that single `unsafe` call is in the FFI crate, behind a CPU check. A quarter of the fuzz cases turn AVX2 off so the portable decoder is compared against C as well.

That version is level with the assembly (0.87× to 1.03×). It is one data point on a simple kernel: a shuffle, a multiply and a shift. It does not show that the large DSP kernels of H.264 or AV1 can be matched the same way.

## What the C decoders do that the ports had to copy

Differential testing against the original means matching it byte for byte, including behaviour that depends on FFmpeg's frame layout rather than on the bitstream:

- Microsoft RLE checks 8-bit and 24-bit runs against the frame's stride, not the picture width, so a run may continue into the padding after a row. A run that does not fit even in the stride is dropped without consuming its pixel value, which is then read as the next code. The port makes the same decisions from the same stride number but writes only the visible part.
- Microsoft RLE treats any packet whose size equals a raw picture as a raw picture, whatever it contains.
- Cinepak strips may start at any x, so the last 4-pixel block of a row can stick out past the end. C writes it through a plain pointer: into the row padding, or, when the stride equals the row length, onto the first pixels of the next row. The port writes to the same byte offsets through a bounds check, so the same visible pixels change.
- Microsoft RLE and Cinepak output a picture even when the packet is damaged.

## A bug this found in the original

For v210 pictures less than four rows tall, FFmpeg's decoder returns the frame without decoding it: its slice count is `av_clip(thread_count, 1, height / 4)`, which is 0 when `height / 4` is 0. Decoding a 48×2 v210 file with the C decoder gives an all-zero frame; the port gives the source picture back. The fuzzer flagged it as a mismatch on its first case with a small height. The fuzz runs above therefore use v210 heights of 4 and up. It has not been reported upstream.

## Do the checks have teeth?

`scripts/mutants.sh NAME` builds deliberately broken variants of a decoder and records which check notices each one. See `results/NAME-mutants.txt`.

| Decoder | Broken variants | Caught by Rust unit tests | Caught by FFmpeg's tests | Caught by 10 s of differential fuzz |
|---|---|---|---|---|
| RPZA | 5 | 0 | 4 | 5 |
| Microsoft RLE | 5 | 4 | 1 | 5 |
| Cinepak | 5 | 0 | 4 | 5 |
| v210 | 6 | 2 | 2 | 6 |

The differential fuzzer caught all 21, each within its first 36 cases. FFmpeg's tests caught 11. They miss what their samples do not contain: the encoders write only some block types and bit depths, and the real samples are few. The variants they missed include both layout-dependent behaviours described above (a Microsoft RLE run bounded by the width instead of the stride, a Cinepak block cut off at the end of its row).

The unit tests are uneven because they were written as smoke tests, not as a safety net: Microsoft RLE's happen to cover what the variants break, RPZA's and Cinepak's do not.

A sixth Cinepak variant (not wrapping a strip's relative position at 16 bits) is listed in `results/cinepak-mutants.txt` as missed by everything. It is not a gap: a wrapped position always ends up above the strip's start and is rejected, exactly as the unwrapped one is, so the variant behaves identically.

## What this does not show

- Three of the four decoders have no assembly, and the fourth has a small kernel. The hard part of FFmpeg, matching hand-tuned SIMD in H.264, HEVC or AV1, is still untested.
- None of these decoders has a known live vulnerability. The two CVEs were fixed years ago, and the C decoders passed the sanitizer runs too. This demonstrates the method, not a bug fix.
- For the three decoders without assembly, the speed-up comes from restructuring the pixel writes, not from the language. The same changes would speed up the C.
- Microsoft RLE's 16-bit and 32-bit modes in `msrledec.c` are not ported: `msrle.c` cannot reach them (two other decoders, AASC and TSCC, can).
- The v210 port does not split a frame across slice threads, as the C decoder can. Benchmarks are single-threaded for both.
- The v210 `custom_stride` option is ported but not fuzzed. Reading the C code, a custom stride smaller than a row makes it read past the end of the packet; that was not run under the sanitizer.
- FFmpeg's encoders only write 8-bit Microsoft RLE and Cinepak and v210 at restricted sizes, so 4-bit and 24-bit RLE and odd sizes are covered by generated streams, not by encoded clips.
- Benchmarks ran in Docker on WSL2, pinned to one core, C built by gcc 12 at FFmpeg's default `-O3` with its x86 assembly enabled, Rust 1.100.
- Log messages differ slightly from the originals; return codes and frames do not.

## Reproduce

Needs Docker. Nothing is installed on the host. The FFmpeg source is not in this repository; fetch it first.

```bash
git clone --depth 1 --branch n8.1.3 https://github.com/FFmpeg/FFmpeg.git ffmpeg
docker build -t ffsafe-poc docker
docker run -d --name ffsafe -v "$PWD:/proj" -v ffsafe-build:/build ffsafe-poc sleep infinity
docker exec ffsafe bash /proj/scripts/build.sh              # Rust + FFmpeg with the four ports
docker exec ffsafe bash /proj/scripts/fate.sh               # FFmpeg's tests for all four
docker exec ffsafe bash /proj/scripts/harness.sh            # harnesses + test clips
docker exec ffsafe bash /proj/scripts/bench.sh cinepak      # benchmark table for one decoder
docker exec ffsafe bash /proj/scripts/fuzz.sh cinepak 180   # differential fuzz
docker exec ffsafe bash /proj/scripts/asan.sh cinepak       # same, under sanitizers
docker exec ffsafe bash /proj/scripts/mutants.sh cinepak    # broken variants
```

The decoder names are `rpza`, `msrle`, `cinepak` and `v210`. `ffmpeg/` is an unmodified shallow clone of FFmpeg tag `n8.1.3`; the build works on a copy inside the container.

## License

LGPL 2.1 or later, the same as the FFmpeg files these ports are derived from (see `LICENSE`). Each Rust decoder is a port of the FFmpeg source file named at the top of its `lib.rs`; the copyright of the original authors, listed in those FFmpeg files, continues to apply. This project is not affiliated with or endorsed by FFmpeg.
