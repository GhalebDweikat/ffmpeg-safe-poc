# TTA (`tta.c`, 441 lines, plus `ttadata.c`, `ttadsp.c` and the SSSE3/SSE4 filter in `x86/ttadsp.asm`)

## Summary

- **FFmpeg tests:** 8 of 8 with the port as the default decoder, on the first build and again at the end: five on real samples (`fate-lossless-tta`, `fate-lossless-tta-encrypted`, which needs the `-password` option, `fate-filter-metadata-silencedetect`, `fate-ffmpeg-error-rate-fail`, `fate-ffmpeg-error-rate-pass`, the last two on a file whose TTA stream cannot be decoded) and three through FFmpeg's encoder (`fate-acodec-tta`, `fate-lavf-tta`, `fate-lavf-mka`). `results/tta-fate.txt`
- **Differential fuzz:** the port as first reported was **not bit-exact**: the coordinator's 10-million-packet verification run found 2 mismatches, both from one bug in the port's bit reader, which my first run of 2,636,156 packets (clean) had not reached. After the fix: 7,712,924 packets (5 processes, 600 s each: 3 on generated streams, 2 mutating `inside.tta` and `encrypted.tta`), 0 mismatches, 0 panics, against the C decoder with its SSE4 filter; 3,668,960 packets produced a frame, 4,043,964 were rejected by both. The two reported seeds, re-run with the fix, pass the cases where they failed. A quarter of the cases run the port's portable filter in place of its SSE one. `results/tta-fuzz.txt`, `results/tta-mismatch/`
- **Sanitizer fuzz:** after the fix, 647,961 packets (6 processes, 120 s each: 3 generated, 3 mutating the other seed clips: the undecodable Matroska sample, a 24-bit clip, an 8-bit 6-channel clip), clean: no mismatches, no panics, no ASan, UBSan or leak reports. That build has no x86 assembly, so there the port is compared against `tta_filter_process_c`. `results/tta-fuzz-asan.txt`
- **Overflow-checked fuzz** (extra, by hand): 1,460,534 packets against a Rust library built with `-C overflow-checks=on`, in which the bug would have been a panic: no panics, no mismatches. `results/tta-fuzz-overflow-checks.txt`
- **Benchmark, Rust time / C time** (C with its assembly, one thread):

  | | straight port (v1) | final (v4) |
  |---|---|---|
  | 10 clips | 1.03 to 1.46 | 0.47 to 0.85 |
  | 10 single-path streams | 1.07 to 1.39 | 0.46 to 0.97 |

  The 0.97 is the all-silence stream; every other path is 0.46 to 0.64. Both tables were measured while other agents were running (load average 11 to 14 for the final one; absolute times were about twice those of the one quiet run), so only the ratios mean anything, and they need the re-measurement on an idle machine. `results/tta-bench-v1.txt`, `results/tta-bench-final.txt`
- **Versions:** 4 (straight port and three optimisation steps).
- **Bugs in the port:** one, and none of my own checks caught it. From v2 on, the bit reader could fill its 64-bit window completely in the last bytes of a packet, and a run of exactly 63 ones starting there then left stale bits in it: a packet refused that C decodes, or wrong samples. It was caught by the coordinator's longer differential fuzz (cases 32,196 and 224,945 of two processes); FATE, my unit tests, my 2.6-million-packet fuzz and the sanitizer run all passed with it in. Fixed (one character), with three new unit tests and a generator change that makes the fuzzer find it within 10 s; see "The bug the first fuzz run missed". Other things that went wrong around the port: four of my test clips labelled "s16" were 24-bit (FFmpeg's encoder picks 32-bit input when the source is float), found by probing the clips; one unit test for Rice parameters above 25 asserted something the format does not guarantee and was rewritten before its first run; two performance bugs (see "Versions").
- **C behaviour that had to be copied:** see "C behaviour copied". The main ones: a frame ends early after exactly `last_frame_length` samples whenever 32 to 39 bits are left, on any packet; bytes after a frame and the frame CRC are ignored unless CRC checking is on, and with checking on but without `explode` a wrong CRC is only logged; one channel is an unspecified layout, not mono; a Rice parameter may grow past 25 as long as it is not the one used; all arithmetic wraps at 32 bits.
- **Reads past the end of the packet:** the C bit reader never returns a bit from there. `get_unary` is limited to the bits left, and before the k remainder bits are read the decoder checks that k bits are left; `get_bits` loads four bytes that can reach up to three bytes into the padding, but only bits inside the packet are used. A run of ones that reaches the end of the packet is not an error in itself in C (with k = 0 decoding even continues, producing samples from no bits), but then nothing is left and the frame fails the final "32 bits left" check. The port reads only the packet and fails at once in that case; the fuzzer sees no difference.
- **Wrong in the C decoder:** nothing found. Both reported mismatches were the port's fault; no input had to be excluded from the fuzzer and the sanitizer run on the C code was clean.
- **Not ported / not covered:** see "Not covered". In short: generated fuzz streams use sample rates up to 3000 (frames up to 3134 samples); larger frames come only from the clips; packets over 4 MB are not generated; the portable (non-AVX2) decoder is correct under fuzz but slower than C on silence (1.21) in the one measurement I made.
- **Changes needed in shared files:** none was required to finish. Things that did not fit audio are listed under "Shared harness".
- **Not verified:** that no other input-dependent bug of the same rarity remains (the fuzz volume is now 7.7 million packets plus the coordinator's 10 million on the old generator, still well below the 100 million and more of the first four ports); the benchmark after the fix (not re-run: the fix is in the byte-at-a-time refill used for the last seven bytes of a packet) and on an idle machine; error codes of a refused `avcodec_open2` (the shim returns the same codes as the C by construction, the fuzzer only compares success or failure); codec context fields set at open other than those a frame carries (`bits_per_raw_sample`); which rare paths the fuzzer's generated streams reach (no coverage measurement; in particular a Rice parameter above 25 that is never used); frame threading beyond one manual run (three clips and the encrypted sample decode to the same MD5 with `-threads 4`, `results/tta-extra-checks.txt`); the two mutation seeds of the plain fuzz run were not also run under the sanitizer and the three of the sanitizer run were not run in the plain build.

## Files

| File | Lines |
|---|---|
| `rust/tta-core/src/lib.rs` | 542 of code without comments (760 in all); two filters, header parsing, both CRCs |
| `rust/tta-core/src/tests.rs` | 15 unit tests, a small TTA encoder and a bit-at-a-time reference reader |
| `rust/tta-core/testdata/` | the two packets the fuzzer found, and the C decoder's output for them |
| `rust/ffsafe-ffi/src/tta.rs` | 304, of which about 130 are a test and a timing probe |
| `shim/tta_rs.c` | 217 |
| `harness/codec_tta.c` | 614, of which about 200 are a TTA encoder |
| `scripts/codecs/tta.sh` | 50 |

## The format

The stream parameters are in a 22-byte "TTA1" header passed as extradata: format (1 plain, 2 encrypted), channels (1 to 16), bits per sample (1 to 24, rounded up to 1, 2 or 3 bytes: output U8, S16, or S32 with the sample in the top 24 bits), sample rate, number of samples. A frame is `256 * rate / 245` samples per channel, so 46,080 at 44.1 kHz; the last one is `data_length % frame_length` long.

A packet is one frame: a little-endian bitstream of one adaptive Rice code per sample, channels interleaved, padded to a byte, then a CRC-32. Per sample: decode the code (a run of ones, a zero, k bits; without a run k is the channel's first parameter, with one its second), adapt both parameters from running sums, map to a signed residual, run an 8-tap sign-LMS filter (the part FFmpeg has assembly for), add a fixed first-order predictor, and after the last channel undo the inter-channel differences. All state is reset at every packet. An encrypted stream differs only in the filter's initial coefficients, which are the bytes of a CRC-64 of the password.

## Versions

Rust/C time, from `bench.sh`; every version passed the fuzz gate. All measured on a loaded machine.

| Version | What changed | Clips | Paths |
|---|---|---|---|
| v1 straight port (`results/tta-lib-v1-straight-port.rs`) | bit reader one bit at a time, as in C | 1.03 to 1.46 | 1.07 to 1.39 |
| v2 (`tta-lib-v2-bit-window.rs`) | 64-bit window, run length from a bit count, branch-free Rice adaptation and filter sign step | 0.75 to 1.04 | 0.70 to 1.26 |
| v3 (`tta-lib-v3-sse-filter.rs`) | filter with SSE intrinsics, state as whole vectors; bit reader state passed by value on the cold paths | 0.68 to 0.90 | 0.64 to 1.24 |
| v4 final (`tta-lib-v4-final.rs`) | Rice limits in one table, parameter picked by index, channel loops compiled separately for mono and stereo | 0.47 to 0.85 | 0.46 to 0.97 |

The v2 numbers are from an `iterate.sh` run with 15 passes and were not saved to a file; the others are in `results/tta-bench-v1.txt`, `-v3.txt`, `-v4.txt` (= `-final.txt`). The saved v2 and v3 sources contain the bit reader bug described below; `tta-lib-v4-final.rs` is the fixed file. The benchmark tables were measured before the fix, which is outside the hot loop.

What was learned:

- The C decoder is slow where codes are unpredictable: it reads the run of ones bit by bit, one hard-to-predict branch per bit, and selects the parameter with another. Counting the run with `trailing_zeros` on a 64-bit window and selecting without branches is where most of the gain on real audio comes from. That gain has nothing to do with the language.
- On silence everything in C is predicted perfectly, and there the branch-free port was 1.2 to 1.3 times slower until v4: the loop is bound by the number of instructions, not by latency, and branch-free code executes both sides. v4 cut the instruction count (one table lookup gives both limits of a parameter; the parameter is loaded by index; with a constant channel count the compiler unrolls the channel loops).
- Two store-to-load stalls, the kind PORTING.md warns about. In v2 the compiler vectorised the portable filter and loaded as a vector the four taps it had just stored one by one. In the first v3 it loaded `error`, `round` and `shift` as one 16-byte vector to get at `shift`, right after storing `error` as 4 bytes. Fixed by keeping the filter state as whole `__m128i` values and passing `shift` and `round` as arguments. The portable filter still has the first stall.
- The SSE filter is the assembly's algorithm written with value-only intrinsics (`_mm_sign_epi32`, `_mm_mullo_epi32`, `_mm_alignr_epi8`), inside functions compiled for AVX2+BMI1+BMI2; the single `unsafe` call is in `ffsafe-ffi/src/tta.rs` behind a CPU check, as for v210.
- Not tried: the filter's latency chain (multiply, horizontal add, back into the taps) can be shortened by keeping the newest output out of the vector: the four newest taps are `out - (sums of older taps)`, so the next sum is `A + out * (qm[4] + qm[5] + qm[6] + qm[7])` with A computed ahead. It was not needed for parity.

Portable decoder (no AVX2), one run of 15 passes, `results/tta-extra-checks.txt`: silence 1.21, the other five rows measured 0.57 to 0.94.

## The bug the first fuzz run missed

The port as first reported was not bit-exact. The coordinator's verification fuzz (12 processes, 400 s, about 10 million packets) found two mismatches that my 2.6 million packets had not reached:

    MISMATCH seed=1010 case=224945 packet=2 what="return codes" size=5x5 bits=8 c=(0,0) rs=(-1094995529,0) len=171
    MISMATCH seed=1005 case=32196 packet=1 what="samples" size=31x25 bits=24 c=(0,0) rs=(0,0) len=4212

Both are one bug, in the port's bit reader (introduced in v2, with the 64-bit window). Near the end of a packet, when fewer than 8 bytes remain outside the window, the window is topped up one byte at a time. That loop ran `while count <= 56`, so from a byte-aligned position it filled the window to 64 bits, one more than the reader's other code allows. A run of exactly 63 ones then removes 64 bits with `acc >>= 64`, which in a release build shifts by 0: the window keeps its old contents, and the next field read gets stale bits. In the first packet that led to a refused packet, in the second to two wrong samples. Both packets end with seven 0xFF bytes and 0x7F, fourteen bytes before the end. The packets and the C decoder's output for them are in `rust/tta-core/testdata/` and `results/tta-mismatch/`.

The fix is one character (`while count < 56`): the window never holds more than 63 bits. It is on the cold path, so the benchmark was not re-run.

Why my checks missed it:

- It needs a run of exactly 63 ones that starts on a byte boundary 8 to 14 bytes before the end of the packet. My generator produced long runs (square waves, arbitrary 32-bit samples, all-ones packets) but nothing aimed at runs about as long as the port's own window, or at the last bytes of a packet. In 10 million generated packets it happened twice.
- The scripts run unit tests with `--release`, where integer overflow wraps silently. In a debug build the shift by 64 is a panic. I never ran the tests in a debug build, and no unit test compared the window reader with a simple one.

What was added:

- `bit_window_matches_bit_at_a_time_reader`: 60,000 random byte strings, mostly 0xFF with 0x7F and 0xFE mixed in, read with random mixes of runs and fields by the window reader and by a bit-at-a-time reader; results, bits left, and the 63-bit limit of the window are compared after every read. It fails on the old code at case 158.
- `run_of_63_ones_in_a_full_window_at_the_end_of_a_packet`: the minimal case, 17 bytes.
- `packets_the_fuzzer_found_decode_like_the_c_decoder`: the two packets against the C decoder's output.
- All unit tests now also pass in a debug build (`cargo test -p tta-core` without `--release`), which the scripts do not run.
- Generator: half of the "arbitrary Rice codes" packets now get runs of 56 to 65 ones (sometimes 40 to 129) in their last one or two codes and in about four other codes. A first attempt put such runs in every fifth code and did not help: each one raises the second Rice parameter by one and little brings it down, so by the end of a packet the generator could no longer write them. With the bug put back as a seventh broken variant, 10 s of fuzzing now finds it at case 2,503; before the generator change it did not find it in 10 s (the original finds were at cases 32,196 and 224,945 of their runs).
- Both reported seeds were run again with the fixed library and the generator as it was: seed 1005 for 143,616 cases and seed 1010 for 331,904 cases, no mismatch (`results/tta-mismatch/after-fix-same-seeds.txt`).

What else of this kind could be hiding: anything that only goes wrong at an exact length or alignment in rarely used code. The bit reader is now compared exhaustively enough against a reference that I trust it; the other rarely used code is the long-run path (`unary_long`, covered by the same test) and the handling of Rice parameters above 25 (covered by one unit test and whatever the fuzzer reaches, not measured).

## C behaviour copied

1. **Early end of a frame.** After each sample of the last channel: if the number of samples so far equals `last_frame_length` and 32 to 39 bits are left, the frame ends there with that many samples. This is tested on every packet, not only the last. A full frame is also accepted where a short one is expected.
2. **End of frame.** After the samples the position is rounded up to a byte and 32 bits must be left, else the packet is refused. Whatever follows is ignored, and the CRC is not looked at.
3. **CRC only on request.** With `err_recognition` CRCCHECK: packets under 4 bytes are refused; a wrong CRC is logged, and refused only if EXPLODE is set too.
4. **Errors give no frame.** Any error while decoding returns `AVERROR_INVALIDDATA` and no frame, however many samples were decoded. Errors are: fewer than k bits left for a remainder; k above 25 on the parameter being used; a value that does not fit 31 bits (`run > INT32_MAX >> k`).
5. **Unused parameters are not checked.** While codes have runs, only the second parameter is read; the first keeps adapting (with `value + shift_1[k0]`, wrapped to 32 bits) and may pass 25, up to 35, where the table stops it. The table is kept verbatim, including its entries of 0x80000000 and 0xFFFFFFFF.
6. **Wrapping arithmetic everywhere**: sums, filter, predictor (`x * 31 / 32` computed in 64 bits and truncated), channel decorrelation (the `/ 2` there rounds toward zero), and the final `* 256` for 24-bit output.
7. **Header:** only the first 22 bytes are read; its CRC is not checked; format 0 is accepted and treated as plain; the sample rate is compared as unsigned, so "negative" rates are refused as too large; bits per sample from 1 to 24 are accepted and only their byte count matters.
8. **Channel layouts:** 2 stereo, 3 2.1, 4 quad, 6 5.1(back), 7 6.1(back), 8 7.1(wide); 1, 5 and 9 to 16 channels get an unspecified layout with that many channels. The container's sample rate and layout are overwritten.
9. **Password:** required for format 2 (open fails with `EINVAL` without one), ignored otherwise; the empty string is a password.

## Bugs found in the C decoder

None. Two things seen while reading, neither run:

- `frame_length too large` in `tta_decode_init` cannot trigger: the sample rate is limited to 0x7FFFFF first.
- A 22-byte header with sample rate 0x7FFFFF and 16 channels makes the decoder allocate a 561 MB decode buffer at open and ask for frames of that size.

## Do the checks have teeth?

`results/tta-mutants.txt`:

| Broken variant | Unit tests | FATE, real samples | FATE, encoder | Differential fuzz (10 s) |
|---|---|---|---|---|
| SSE filter: one sign-step constant wrong | missed | caught | caught | caught, case 2 |
| portable filter: wrong mask for dx[7] | caught | missed | missed | caught, case 1 |
| value overflow check dropped | missed | missed | missed | caught, case 147 |
| second Rice parameter adapted when unused | caught | missed | caught | caught, case 2 |
| 8-bit streams use the 16-bit predictor | caught | missed | missed | caught, case 1 |
| 31 bits accepted in place of the CRC | missed | missed | missed | caught, case 95 |
| end of packet: bit window may fill to 64 bits (the real bug, put back) | caught | missed | missed | caught, case 2503 |

This is the table after the bug fix, with the new unit tests and the changed generator. The fuzzer caught all seven; FATE three between its two groups; the unit tests four. The seventh variant is the bug described above: with the first generator and unit tests, nothing here caught it in 10 s, and FATE cannot (it passed with the bug in). The unit-test column only runs the core crate's tests, which cannot call the SSE decoder (it needs `unsafe`); the test that compares the two filters is in `ffsafe-ffi` and is not counted. FATE never runs the portable filter and has no 8-bit sample.

## Not covered

- Generated fuzz streams have sample rates of 1 to 3000 (frames of 1 to 3134 samples per channel). Frames of 8,359 to 100,310 samples are covered by the seed clips and benchmark clips only.
- Packets above the harness limit of 4 MB, and the size above which `init_get_bits8` refuses a packet (about 256 MB; the shim has the same test).
- Whether generated streams reach every rare path was not measured. The generator writes Rice codes with values of up to 31 bits and runs of up to 3000 ones, and all-ones packets, so that large parameters and overflow are reached (the "overflow check dropped" variant was caught), but there is no coverage report.
- Non-x86 builds have only the portable filter. On x86-64 without AVX2, BMI1 and BMI2 the portable filter is used even if SSE4.1 is there.
- Frame threading: the capability is kept, as in the C (each thread gets its own decoder object, and a decoder keeps nothing between packets). Checked by hand only.
- Log messages have the same text as the C decoder's but a different class name.
- The benchmark has no encrypted clip (the benchmark cannot pass a password for a file); the `s16-encrypted` path covers it.

## Shared harness

Nothing blocked the port. What did not fit audio, worked around in my own files:

- `iterate.sh` starts 4 generated-stream processes plus one per seed clip, with no cap. To stay at 6 processes my description file lists two seeds by default and takes others from `TTA_FUZZ_SEEDS`. A process cap as an argument would be cleaner.
- `bench.sh` and `bench.c` print `size` (0x0 for audio files) and time per frame. For audio the useful figures are sample rate, channels and time per sample. I used `SYNTH_SIZE=44100x1` to pass the sample rate to `synth` as the "width".
- `bench.c` gives `setup` an all-zero `Cfg` for files, so a file that needs a decoder option (here: the password of `encrypted.tta`) cannot be benchmarked.
- The fuzzer compares frames, not the codec context after `avcodec_open2`, and treats open as success or failure only. For a decoder that sets its parameters from extradata, comparing `sample_fmt`, `sample_rate`, `ch_layout`, `bits_per_raw_sample` and the error code would be a small addition to `difffuzz.c`.
- `mutants.sh` counts only `cargo test -p NAME-core` as unit tests; tests of the SIMD path have to live in `ffsafe-ffi` and are not run.
- `fixup` gets a zero `Cfg` in seed mode apart from `opt`, which is enough here (the CRC needs no stream parameters).
- `build.sh`, `iterate.sh` and `mutants.sh` run unit tests only with `--release`, and the fuzzed library is a release build, so an integer overflow in a port wraps silently. The bug described above is a panic in a debug build. Two cheap additions would have caught it: run `cargo test -p NAME-core` once without `--release`, and fuzz a library built with `RUSTFLAGS="-C overflow-checks=on"` next to the sanitizer run (a panic is already counted as a failure through `FFSAFE_PANIC`). I did the second by hand once, see `results/tta-fuzz-overflow-checks.txt`.
- The fuzzer does not print a case's extradata or options, so a mismatch in a generated stream cannot be replayed outside the fuzzer without them. My `setup` hook prints them to stderr when `TTA_DEBUG` is set.
