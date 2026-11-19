# QuickTime Animation (RLE): port notes

C source: `ffmpeg/libavcodec/qtrle.c` (593 lines). Port: `rust/qtrle-core/src/lib.rs` (667 lines of code without tests), registered as `qtrle_rs`.

## Summary

- **FFmpeg's tests:** 17 of 17 pass with the port as the default decoder (`results/qtrle-fate.txt`). They are the seven depth tests on real samples (`fate-qtrle-1bit` to `-32bit`), three other tests that decode QuickTime RLE samples (`fate-rgb24-mkv`, `fate-filter-fps`, `fate-filter-fps-cfr`), and seven encoder round trips (`fate-vsynth{1,2,3,_lena}-qtrle`, `fate-vsynth{1,2,_lena}-qtrlegray`). They passed on the first build.
- **Differential fuzz:** 56,035,740 packets, 0 mismatches, 0 panics (`results/qtrle-fuzz.txt`; 6 processes for 300 s: 3 on generated streams, 3 mutating real packets of a 1-bit sample, a 4-bit sample and a 16-bit encoded clip). 8,752,959 of the packets were rejected by both decoders with the same error code.
- **Sanitizer fuzz:** 11,119,808 packets under AddressSanitizer and UBSan, clean: no reports, no mismatches (`results/qtrle-fuzz-asan.txt`).
- **Speed, Rust time / C time, on clips:** straight port 1.73x to 21.5x; final 0.29x to 1.06x. **Per decoder path:** straight port 0.87x to 54x; final 0.10x to 0.95x. The slowest final clip rows are `real-16bit` 1.06x, `real-24bit` 1.01x and `mandelbrot-720p-rgb555` 1.00x; everything else is below 1.0. The straight-port numbers were taken with nine other jobs on the machine and 9 passes, the final ones with one other job and 31 passes (`results/qtrle-bench-v1.txt`, `results/qtrle-bench-final.txt`). Neither was measured on an idle machine.
- **Versions:** 4 (straight port, row slices, local reader, skip-only lines).
- **Bugs in the port:** none reached a check. FFmpeg's tests passed on the first build and the first working fuzz run was clean. One mistake was caught by reading the change back before building it (see "Bugs").
- **C behaviour that had to be copied:** the picture is one long buffer of `stride * height` bytes, so runs and skips cross from a row through its padding into the next row; writes may end in the padding of the last row; the 1-bit mode changes one row more than the header says; reads past the end of the packet give zeros in the groups the C code reads in; a damaged packet still gives a picture. Details below.
- **Wrong in the C decoder:** nothing memory-unsafe found, and the sanitizer run is clean. One thing worth knowing: for streams with runs that leave their row, the decoded picture depends on the frame's stride, which the caller's allocator chooses. See "Findings about the C decoder".
- **Not ported / not covered:** log messages differ (the port does not print the pixel position). Strides other than the default allocator's were not tested. Big-endian hosts were not run. The 2-bit, 8-bit, 16-bit, 24-bit and 32-bit real samples and three of the four encoded seed clips are not in the long fuzz run; seven of those eight had one 8-second gate run each on an earlier version, and the 32-bit real sample was never a fuzz seed.
- **Changes needed in shared files:** `scripts/iterate.sh` should stop when `cargo build` fails; today it carries on and fuzzes and benchmarks the previous library. `scripts/codecs/qtrle.sh` still has `STATUS=wip`. The README tables have no qtrle column.
- **Not verified:** see the last section.

## Files

| File | Lines |
|---|---|
| `rust/qtrle-core/src/lib.rs` | 667 of code, plus 14 unit tests |
| `rust/ffsafe-ffi/src/qtrle.rs` | 76 |
| `shim/qtrle_rs.c` | 142 |
| `harness/codec_qtrle.c` | 262 |
| `scripts/codecs/qtrle.sh` | 52 |

The Rust is longer than the C because each depth has two forms of every code: a fast one for the case where the pixels all land in the visible row and the bytes are all in the packet, and a slow one that follows the C code step by step for everything else.

## How the port is wired

The C decoder checks the packet header before it asks FFmpeg for a frame, and three outcomes are possible: decode, repeat the previous picture, or reject the packet. So the shim calls Rust twice:

- `qtrle_rs_check(packet, height, discard_damaged_percentage)` returns decode, duplicate or invalid. It makes the C decoder's checks in the C decoder's order: fewer than 8 bytes is a duplicate; a declared size that exceeds the packet by more than the allowed percentage is invalid; a region header in fewer than 14 bytes, or a region that does not fit the picture, is a duplicate.
- `qtrle_rs_decode(packet, frame, ...)` reads the header again (it does not trust numbers handed back through C) and decodes onto the visible picture.

The shim does what `qtrle.c` does around that with the FFmpeg API: `ff_reget_buffer` for a decoded packet, `ff_decode_frame_props` for a duplicate, `AVERROR_INVALIDDATA` for a duplicate when there is no previous picture, and the palette from packet side data for the eight palette depths. It never reads packet bytes or writes pixels.

## Versions

| Version | What changed | Clips, Rust/C | Paths, Rust/C | Saved as |
|---|---|---|---|---|
| 1 | Straight port. Every byte written through a function that checks `position % stride < row length`. | 1.73 to 21.5 | 0.87 to 54 | `qtrle-lib-v1-straight-port.rs`, `qtrle-bench-v1.txt` |
| 2 | One generic line decoder for 2 to 32 bits. It keeps a slice of the visible row; a code whose pixels all land in that slice and whose bytes are all in the packet is written directly, anything else goes to the C-exact slow path. 1-bit pixels are unpacked with one multiplication per byte. | 0.29 to 1.13 | 0.11 to 5.1 | `qtrle-lib-v2-row-slices.rs`, `qtrle-bench-v2.txt` |
| 3 | The stream is a local slice instead of a `Reader` passed by reference (the reference kept it in memory, which put a store and a load into the loop that finds the next code). Copies up to 32 bytes use two overlapping fixed-size moves instead of `memcpy`. 2-bit and 4-bit run patterns are built as one integer. 24-bit runs read the pixel as one 32-bit word and store two pixels as a 4-byte and a 2-byte write. One function per depth instead of everything inlined into one. | 0.28 to 1.08 | 0.12 to 1.25 | `qtrle-lib-v3-local-reader.rs`, `qtrle-bench-v3.txt` |
| 4 | A line that ends right after its skip byte is finished before the row slice is looked up. | 0.29 to 1.06 | 0.10 to 0.95 | `qtrle-lib-v4-final.rs`, `qtrle-bench-final.txt` |

Versions 1 to 3 were measured with 7 or 9 passes on a busy machine; treat their numbers as rough. `qtrle-bench-v2.txt` also holds the fuzz gate output of that turn.

What mattered, in the order found:

- The straight port was slow only because of the per-byte modulo. With that gone (version 2) most rows were at parity or faster.
- In version 2 the 2-bit run path was 5 times slower than C. The 16-byte pattern was assembled with four 4-byte stores and then loaded as one 16-byte value for every fill; building it as a `u128` fixed it.
- The 24-bit run was written as `*px = [r, g, b]`. The compiler turned that into three single-byte stores per pixel, and a version with `[r, g, b, r, g, b]` into six. Building the pattern from separately loaded bytes did not help: the compiler saw through `u32::from_ne_bytes([r, g, b, r])` and stored bytes again. What worked was loading the pixel together with the byte after it as one 32-bit word and shifting; then the stores are a 32-bit and a 16-bit write.
- Passing the stream reader by `&mut` to the (cold) slow-path functions made the compiler keep the reader in memory in the hot loop. A local slice, wrapped in a `Reader` only when the slow path is taken, stays in registers.
- The all-skip frame (`skip24`) was 1.25x to 1.35x in versions 2 and 3 and is 0.93x in version 4.

No SIMD intrinsics were needed.

## C behaviour that had to be copied

1. **The picture is one buffer.** Each C function computes `pixel_limit = linesize * height` and checks only that a write ends at or before it and that the position is not negative. A run, copy or skip may therefore leave its row, pass through the padding and continue on the next row, and may end in the padding after the last row. The port keeps the same position, makes the same checks with the same stride, and drops the bytes that fall in padding. The broken variants "limit is the visible end" and "write stops at the row end" both differ from C within 6 fuzz cases, so the C decoder really does this.
2. **Skip code 0 goes backwards.** A skip byte is one more than the number of units to skip, so 0 moves one unit back, possibly into the previous row. It is only refused if it would go before the start of the buffer.
3. **1 bit per pixel changes one row more than asked.** `qtrle_decode_1bpp` counts `lines_to_change + 1` "next line" codes (the comment in the C refers to ticket 226). With a region header for N lines, a stream with N + 1 lines changes N + 1 rows. The variant without the extra line fails `fate-qtrle-1bit` and the fuzzer.
4. **Short packets read zeros, in groups.** `bytestream2_get_ne16/ne32/ne64` return 0 and jump to the end of the packet when fewer bytes than asked for are left. A 24-bit copy reads two pixels as 4 + 2 bytes and an odd pixel as 2 + 1; a 32-bit copy reads 8, then 4; a 16-bit pixel reads 2. So a packet that ends in the middle of a pixel gives zeros for the whole group. The 8-bit copy is different again: it copies what is there and leaves the rest of the destination untouched, but still advances by the full length.
5. **A code that is the last byte of the packet ends decoding** (2 to 32 bits), before it is interpreted. The 1-bit mode has no such check.
6. **A damaged packet still gives a picture**, and the return value is the packet size. The only rejections are the size check against `discard_damaged_percentage` and a duplicate packet with no previous picture.
7. **16-bit pixels** are big-endian in the stream and stored in native order (`AV_PIX_FMT_RGB555`).
8. **The palette** is updated from packet side data only for packets that are decoded, not for duplicates.

## Findings about the C decoder

Nothing memory-unsafe: 11.1 million packets under AddressSanitizer and UBSan without a report.

**The decoded picture can depend on the stride.** Because of point 1 above, where the pixels of an over-long run land depends on `linesize`, not on the picture width. `linesize` comes from the buffer allocator: FFmpeg's default pads rows to a CPU-dependent alignment, and an application can install its own `get_buffer2`. The same packet can so decode to different pictures in different programs. Only streams that are damaged or not written by a normal encoder are affected; FFmpeg's encoder never writes a run past the end of a row.

To see it: an 8-bit, 4x2 stream whose packet body is `01 FD 01 02 03 04 FF` (skip to column 0, a run of three 4-pixel groups, end of line). With the default allocator's stride (which pads a 4-byte row to well over 12 bytes; I did not print it) all twelve bytes go to row 0, four visible and eight in padding, and row 1 is untouched. With a stride of 8 the third group lands on row 1; with a stride of 4 the run is refused as a whole (12 bytes against a limit of 8) and nothing is written. The unit test `run_crosses_padding_into_next_row` shows the stride-8 case in the port. I did not run the C decoder with a custom `get_buffer2`; this is from reading the code, and from the port matching C at the default stride only when it uses the stride too.

**The 1-bit extra line** (point 3) is deliberate according to the comment, but it means a region header of N lines does not limit the change to N rows.

Neither has been reported upstream.

## Bugs

In the port:

- None found by any check.
- Caught before the first build of version 3: when `run` was changed to receive the rest of the packet instead of exactly one unit, three functions still began with `let &[a, b, c, d] = src else { return };`, which would have made every 2-bit, 4-bit and 16-bit run write nothing. Found by reading the change back.

In my harness and description files:

- `harness/codec_qtrle.c`: a row that had already run past its end could get a negative remaining length, which turned into a copy code of about four billion units and a 4 MB packet. Nothing was wrong with either decoder, but the first fuzz gate did 64 cases in 8 seconds. Noticed from the case count; fixed.
- `scripts/codecs/qtrle.sh`: I listed `fate-vsynth3-qtrlegray`, which FFmpeg does not have. `fate.sh` reported it as a failure ("No rule to make target").
- FFmpeg's encoder refuses grey video whose width is not a multiple of 4, so the grey seed clip is 68 pixels wide, not 65.

In the shared scripts:

- `iterate.sh` does not stop when `cargo build` fails. After a compile error it relinked the harness against the previous library and printed a clean fuzz gate and a benchmark for code that was not the code on disk. The compiler error was in the output, a few lines above the clean-looking results; I saw it there.

## Do the checks have teeth?

`results/qtrle-mutants.txt`, eight broken variants:

| Variant | Unit tests | FFmpeg, real samples | FFmpeg, encoder | Fuzz, 10 s |
|---|---|---|---|---|
| 16-bit copy not byte-swapped | caught | caught | missed | case 15 |
| 1-bit: extra line not counted | caught | caught | missed | case 3 |
| Limit is the visible end, not stride x height | caught | missed | missed | case 6 |
| Skip code off by one | caught | caught | caught | case 2 |
| `discard_damaged_percentage` fixed at 95 | caught | missed | missed | case 26 |
| 24-bit short packet: zeros per byte, not per group | caught | missed | missed | case 364 |
| Write stops at the row end instead of continuing | missed | missed | missed | case 2 |
| 4-bit pixels swapped | caught | caught | missed | case 7 |

Fuzz 8 of 8, unit tests 7, FFmpeg's tests on real samples 4, FFmpeg's encoder tests 1. The unit tests miss the seventh because their test of a run crossing rows uses a stride that is a multiple of the unit size, so no single unit straddles the row end.

## The fuzz generator

`harness/codec_qtrle.c` writes a header (sometimes with flag bits, sometimes with a region that is slightly outside the picture, sometimes with a wrong declared size), then lines of skip, run and copy codes for the configured depth. One row in twelve runs past its end by up to 70 units, lines are sometimes left unterminated, and the line count is sometimes off by one. Depths are 1, 2, 4, 8, 16, 24, 32, 33, 34, 36 and 40, and now and then one the decoder refuses. A quarter of the cases set `discard_damaged_percentage` to 0, 1, 50, 94, 99 or 100. A sixth of the packets carry a random palette. Run codes are 2 to 127 units and copy codes 1 to 127; a run of 128 (code 0x80) is reached only through mutation, for which 0x80 is one of the preferred byte values.

`FUZZ_SEEDS` defaults to three clips so that `fuzz.sh qtrle N 3` starts six processes. `QTRLE_SEEDS="..."` in the environment picks others.

## Benchmark clips

FFmpeg's encoder writes 8-bit grey (decoded as palette video), 16, 24 and 32 bits, so the encoded clips cover those. 1, 2 and 4 bits are timed on the three real samples (320x240, 38 frames each) and on generated single-path streams. The path rows named `-short` use pieces of two units (runs) or three (copies); the others use 127.

## Not ported, not covered, not verified

- **Log messages.** The C decoder prints the pixel position when a write does not fit; the port prints a fixed message. Return codes and frames are the same.
- **Other strides.** Everything was compared with the stride FFmpeg's default allocator gives. A caller with its own `get_buffer2` was not tested, in either decoder.
- **Big-endian hosts.** The 16-bit path uses native byte order like the C; it was only run on x86-64.
- **Seeds in the long run.** The 300-second run mutates the 1-bit and 4-bit real samples and one 16-bit encoded clip. The 2-, 8-, 16- and 24-bit real samples and the 24-bit, grey and 32-bit encoded seed clips were each used in one 8-second gate during development, on versions 1 to 3, not on the final code. The 32-bit real sample was never a fuzz seed. All depths are in the generated streams of the long run.
- **Picture sizes.** Generated streams are at most 250 pixels wide and high; larger pictures are covered only by the seed clips, FFmpeg's tests and the benchmark's identical-output check on every clip (up to 1280x720).
- **Pictures of 2 GB and more.** `Frame::new` refuses a picture whose `stride * height` does not fit a C `int`, and the shim then returns `AVERROR_BUG`. FFmpeg's own size check keeps pictures below that, so I believe this cannot be reached; I did not try.
- **The per-path and per-clip speed numbers** were measured with other work running on the machine and should be measured again on an idle one.
- **Frame threading.** The C decoder does not declare it; neither does the port.
