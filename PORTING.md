# Porting a decoder

How to add one more FFmpeg decoder to this project. The four finished ports are the worked examples: `cinepak` for a decoder that keeps state between frames, `v210` for one with SIMD, `msrle` for palettes and several bit depths, `rpza` for the simplest case.

## The files of a port

For a decoder called `NAME`:

| File | What it is | Size in the examples |
|---|---|---|
| `rust/NAME-core/` | The decoder. `#![forbid(unsafe_code)]`, no dependencies. All bitstream parsing and every sample write. | 260 to 430 lines of code |
| `rust/ffsafe-ffi/src/NAME.rs` | C entry points. Turns (pointer, length) pairs into slices, catches panics. The only place `unsafe` is allowed. | 60 to 95 |
| `shim/NAME_rs.c` | FFmpeg glue: registers `NAME_rs`, gets the frame from FFmpeg, calls Rust, maps the result to FFmpeg return codes. Never reads packet bytes or writes samples. | about 105 |
| `harness/codec_NAME.c` | Packet generators for the fuzzer and the benchmark (a `CodecOps`, see `harness/common.h`). | 80 to 210 |
| `scripts/codecs/NAME.sh` | FFmpeg's tests for the decoder, the samples they need, test clips, broken variants. | about 25 |
| `notes/NAME.md` | What happened: versions and their speed, C behaviour that had to be copied, bugs found, what is not covered. | |
| `results/NAME-*` | Raw output of every check, and the earlier versions of the Rust. | |

A new decoder also needs one line each in `rust/Cargo.toml` (workspace member), `rust/ffsafe-ffi/Cargo.toml` (feature and dependency), `rust/ffsafe-ffi/src/lib.rs` (`mod`), and `harness/common.h` and `harness/common.c` (the `ops_NAME` declaration and list entry).

## Rules

1. **Bit-exact with the C decoder, through the libavcodec API.** Same return codes from `avcodec_send_packet` and `avcodec_receive_frame`, same frames: format, size, flags, picture type, colour properties, every visible sample, the palette; for audio the sample format, sample count, sample rate, channel layout and every sample. This includes damaged input: where the C decoder outputs a partly decoded frame, so does the port; where it rejects a packet, so does the port, with the same error code.
2. **No `unsafe` in the core crate, no dependencies, and it must not panic on any input.** A panic is caught at the FFI boundary and reported as `FFSAFE_PANIC`, which the fuzzer counts as a failure.
3. **Rust sees only what a caller can see.** The shim passes the packet and the visible part of each plane (`stride * (rows - 1) + visible row length`). Where the C decoder writes into row padding or into rows below the picture, the port drops those writes, and reproduces any effect they have on visible samples.
4. **The comparison is not weakened to make a port pass.** If the C decoder's output depends on something a caller cannot control (uninitialised memory, a read past the packet, undefined behaviour), write it down in `notes/NAME.md` with a way to reproduce it, and keep the fuzzer away from exactly that input and nothing more (see the height restriction in `harness/codec_v210.c`). That is a finding, not a failure.
5. **Single-threaded.** The ports do not split a frame across threads, and all comparisons and benchmarks run both decoders with one thread. Do not declare threading capabilities the port does not implement (v210 keeps frame threading because there it costs nothing).
6. **SIMD only through safe intrinsics.** Inside a function marked `#[target_feature(enable = "avx2")]` the value-only intrinsics of `core::arch::x86_64` are safe to call, so the core crate can still forbid `unsafe`. The one `unsafe` call of that function goes in the FFI module behind `is_x86_feature_detected!`. Keep a portable version, a unit test that the two agree, and a test hook so the fuzzer runs both (see `v210`). The machine has AVX2 but not AVX-512. Try plain Rust first: three of the four finished ports needed no intrinsics.
7. **The pristine FFmpeg clone in `ffmpeg/` is never edited.** Read the C there; the build works on a copy.

## The loop

Everything runs inside the Docker container (`ffsafe`); the project is mounted at `/proj` and build output goes to the volume at `/build`.

```bash
docker exec ffsafe bash /proj/scripts/build.sh            # Rust library + FFmpeg with every port
docker exec ffsafe bash /proj/scripts/fate.sh NAME        # FFmpeg's own tests, port as the default decoder
docker exec ffsafe bash /proj/scripts/harness.sh          # fuzzer, benchmark, test clips
docker exec ffsafe bash /proj/scripts/iterate.sh NAME     # rebuild Rust, unit tests, 8 s fuzz gate, benchmark
docker exec ffsafe bash /proj/scripts/fuzz.sh NAME 180 6  # long differential fuzz
docker exec ffsafe bash /proj/scripts/asan.sh NAME 90 4   # same under AddressSanitizer + UBSan
docker exec ffsafe bash /proj/scripts/mutants.sh NAME     # deliberately broken variants
docker exec ffsafe bash /proj/scripts/bench.sh NAME 31    # benchmark table only
```

1. **Read the C decoder completely**, including the helpers it calls (bit readers, VLC tables, DSP functions and their x86 assembly in `libavcodec/x86/`), the FATE tests that cover it (`grep -rn NAME tests/fate/`), and how it is registered (capabilities, options, pixel or sample formats).
2. **Write the description file** `scripts/codecs/NAME.sh` first: `C_DECODER`, `CONFIG`, `FATE_REAL` (tests on real samples), `FATE_ENC` (tests that go through FFmpeg's encoder, if it has one), `FATE_SAMPLES` (paths under `fate-suite.ffmpeg.org`, the first being a file of this codec), and `make_clips` (benchmark clips made with FFmpeg's encoder via `enc`, real samples via `real`; names starting with `seed-` are small clips used only as fuzz seeds, listed in `FUZZ_SEEDS`). If the clips are not in AVI or QuickTime, set `DEMUXERS` (and `PARSERS`) so the sanitizer build can read them. The NUT container (`.nut`) holds any codec.
3. **Straight port.** Port the C as directly as you can, without optimising. Write a few unit tests as you go (hand-made packets, edge sizes). Save a copy as `results/NAME-lib-v1-straight-port.rs` (or a directory, for a crate with several files).
4. **FFmpeg's tests** (`fate.sh NAME`). They must all pass with the port as the default decoder. Save the output as `results/NAME-fate.txt`.
5. **Harness.** Write `harness/codec_NAME.c`: `config` picks the stream parameters for a fuzz case, `gen` writes one mostly valid packet (it must set `b->n` itself), `synth` builds single-path streams for the benchmark where that makes sense. For bitstreams too complex to generate by hand, a simple generator plus real packets as seeds is fine; the fuzzer mutates both. Use `fixup` to repair checksums after mutation, and `options`/`setup` for decoder options. Sizes matter: cover odd sizes, 1-pixel pictures, and rows that exactly fill the stride.
6. **Differential fuzz** (`iterate.sh NAME 0` for a quick gate, `fuzz.sh` for long runs). Every mismatch is either a bug in the port or behaviour of the C decoder to be copied. A mismatch prints the seed and case number, saves the packet as `mismatch.bin`, and is reproducible by running `difffuzz NAME SEED SECONDS` again with the same seed.
7. **Benchmark** (`bench.sh`). The table gives decode time per frame for C and Rust and their ratio, per path and per clip. Save the first one as `results/NAME-bench-v1.txt`.
8. **Optimise** until every row is at parity (Rust/C at or below about 1.05; run-to-run variation is up to 10%). After every change the fuzz gate must pass again. What worked before: keep a slice of the current row instead of recomputing offsets; write fixed-size chunks (`as_chunks_mut`, `first_chunk`, `split_first_chunk`) so bounds checks disappear; build pixels in 32- or 64-bit words; do arithmetic in the narrowest type so the compiler vectorises it; avoid reading back what was just written with a different width (store-to-load stalls). Save each kept version in `results/`.
9. **Final checks.** `fate.sh` again, a long fuzz (`results/NAME-fuzz.txt`), the sanitizer fuzz (`results/NAME-fuzz-asan.txt`), the final benchmark (`results/NAME-bench-final.txt`), and broken variants: put five or six one-line bugs in `MUTANTS` in the description file (wrong constant, off-by-one, a skipped branch, a copied C quirk removed), run `mutants.sh`, and save the table as `results/NAME-mutants.txt`. A variant no check catches is either a gap in the fuzzer's generator, to be fixed, or equivalent code, to be explained.
10. **Write `notes/NAME.md`.**

## Several decoders at once

Each decoder can be built in its own tree, so several can be worked on at the same time without sharing a build:

```bash
docker exec -e FFSAFE_WORK=NAME -e FFSAFE_JOBS=6 -e FFSAFE_CPU=4 ffsafe bash /proj/scripts/build.sh
```

`FFSAFE_WORK=NAME` makes every script use `/build/w/NAME` (a copy of the main FFmpeg tree, its own Rust target directory, harness, clips and sanitizer build) and build only that decoder plus the finished ones. A decoder that is still being worked on has `STATUS=wip` in its description file, which keeps it out of the others' builds. `FFSAFE_JOBS` limits `make`, `FFSAFE_CPU` is the CPU benchmarks are pinned to. In this mode, touch only the files of your own decoder.

## Things that have gone wrong before

- Shell scripts must have LF line endings.
- A unit test packet that happened to have a special size took a different decoder path than intended. Check which path a test really exercises.
- FFmpeg's tests passing means little on its own: they passed with a real bug in the v210 port, because none of their sizes reached it. The differential fuzzer has caught every deliberately broken variant so far; FFmpeg's tests about half.
- Benchmarking a stream that makes the decoder log an error times the logging. The benchmark sets the log level to quiet, but check that a synthetic stream is valid.
- The C decoders bound some writes by the frame's stride, not its width, and some write blocks that stick out past the end of a row. Both show up as fuzz mismatches at sizes where the row exactly fills the stride.
- gcc 12's AddressSanitizer sometimes dies in a "DEADLYSIGNAL" loop before `main`; `fuzz.sh` retries those starts.
