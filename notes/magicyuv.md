# MagicYUV

Port of `libavcodec/magicyuv.c` (694 lines) with the parts of `vlc.c`, `bitstream_template.h` and `lossless_videodsp.c` it uses. FFmpeg has x86 assembly for the predictors (`x86/lossless_videodsp.asm`, 351 lines, up to AVX2). The Rust decoder is 1,104 lines of code in four files (`lib.rs` 589, `vlc.rs` 207, `pred.rs` 202, `bits.rs` 106), plus 223 in the FFI module and 194 in the shim. The packet generator for the harness is 514 lines.

## Summary

- **FFmpeg's tests:** 11 of 11 pass with the port as the default decoder (7 on real samples, 4 through FFmpeg's encoder). They passed on the first build.
- **Differential fuzz:** 10,896,262 packets on the final code (6 processes, 300 s), 0 mismatches, 0 panics. 5,267,962 of them produced a frame. An earlier run of the same length on version 2 was also clean (10,335,737 packets). Only 3 of the 6 processes generate streams, and they are slow (about 1,100 packets a second each): about 1.2 million of the packets are generated ones, the rest are mutations of real and encoded packets.
- **Sanitizer fuzz:** 1,874,461 packets under AddressSanitizer and UBSan (6 processes, 90 s), clean. That build has no x86 assembly, so it also compares the port with FFmpeg's C versions of the predictors.
- **Speed, Rust time / C time, single thread, C with its AVX2 assembly:**
  - Straight port: clips 0.90× to 1.33×, paths 0.85× to 3.12×.
  - Final: clips 0.77× to 0.87× (20 clips), paths 0.74× to 1.38× (13 paths; 11 of them 0.74× to 0.94×).
  - Rows above parity in the final version: raw 8-bit slices, 1.38× (0.59 ms against 0.43 ms a frame at 1280×720; 1.53× on the fastest passes), because the compiler turns the vector load of the left predictor into five instructions where FFmpeg's assembly has one, and safe code has no way to ask for the load directly. Raw 16-bit slices are at 1.07× (1.10× to 1.25× in earlier, noisier runs), which is within the run-to-run variation. Why the coded rows are 10 to 25% faster than C was not investigated; the reader is a call-for-call copy of the C one.
- **Versions:** 5 (straight port, SIMD predictors, raw rows predicted as they are read, shorter carry chain in the running sums, vector loads).
- **Bugs in the port:** none found by any check. The first fuzz run was clean. Two mistakes of mine were caught on the way, neither in the decoder: a unit test with code lengths the table builder rightly refuses, and a suspected bug in FFmpeg's joint table that a test program run against the C decoder disproved (see below).
- **C behaviour copied:** a damaged slice still gives a frame, with unread rows left as they were in the frame buffer and the predictor run over them; the bit reader takes in up to 7 bytes after the end of a slice; a joint table hit stores 8 bytes whatever its number of symbols; bits that match no code end the slice in one place and give sample -1 in another; the table builder's rules for which sets of code lengths are accepted. Details below.
- **Wrong in the C decoder:** nothing that changes the output of valid streams. Three things worth knowing, none reproduced with a standalone program: the output for damaged slices depends on what the frame buffer held before; two integer overflows are possible in theory for very large pictures. See below.
- **Not ported, not covered:** slice threading; frame threading is declared but untested; packets whose padding is not zero; pictures larger than 250×250 in the fuzzer; the 10, 12 and 14-bit formats have no sample and no encoder in FFmpeg, so they are checked against the C decoder on generated streams only.
- **Shared files:** no change needed. Two things to know: `fate.sh` does not relink `ffmpeg`, so `build.sh` has to run after a Rust change before FFmpeg's tests mean anything; `iterate.sh` starts 4 fuzz processes plus one per seed file, which is why this decoder has three seed files, each joined from several clips.
- **Not verified:** see the last section.

## Versions

Whole-clip and per-path numbers are Rust time / C time. Versions 1 and 2 were measured while nine other ports were running on the machine, the rest with one other; the final table was measured last.

| Version | What changed | Clips | Paths | Saved as |
|---|---|---|---|---|
| 1 | Straight port: the C bit reader copied call for call, scalar predictors | 0.90 to 1.33 | 0.85 to 3.12 | `results/magicyuv-lib-v1-straight-port/`, `magicyuv-bench-v1.txt` |
| 2 | Predictors with AVX2 intrinsics: left and gradient (8-bit) and left (16-bit) as running sums, 32 or 16 samples at a time; median (8-bit) one sample at a time but in vector registers, as FFmpeg's SSE2 assembly does | 0.78 to 0.87 | 0.77 to 1.21 | `results/magicyuv-lib-v2-simd-predictors/`, `magicyuv-bench-v2.txt` |
| 3 | Raw slices: each row is predicted right after it is copied | not measured | raw8 1.60, raw10 1.12, others 0.81 to 0.92 | `magicyuv-bench-v3-paths.txt` |
| 4 | Running sums: the sums within a vector no longer wait for the carry from the previous one. Also tried: reading raw 16-bit samples by bit position instead of through the reader | not measured | raw8 1.42, raw10 1.55 (worse; the positional read was taken out again) | not saved |
| 5 | Vector loads built the way the compiler turns into one instruction, per function (see `pred.rs`) | 0.77 to 0.87 | 0.74 to 1.38 (raw8 1.38, raw10 1.07, others 0.74 to 0.94) | `results/magicyuv-lib-v5-final/`, `magicyuv-bench-final.txt` |

What made the difference was version 2. In the straight port the Huffman reading was already level with C (left predictor: 0.94 to 0.99), and the predictors were what was slow: gradient 1.24 to 1.33, median 1.17 to 1.32 on clips.

The median predictor cannot be done several samples at a time, because each sample depends on the one before. In scalar code a byte minimum or maximum is a compare and a conditional move, two steps of the chain; in a vector register it is one. Version 2 does what the assembly does, with intrinsics: load 16 samples, work through them in the low byte of the registers, store 16.

Raw slices stayed above parity. FFmpeg copies the rows and runs its AVX2 left predictor; the port does the same with intrinsics and takes about 1.4 times as long on a path that costs 0.3 to 0.6 ms a frame (coded frames cost 4 to 20 ms). The generated code for the load in the left predictor is five instructions instead of one; building the vector differently made it worse (32 byte inserts, 3.2 times C). The intrinsics that load from a pointer are not available to safe code, so the port depends on the compiler recognising a vector built from array elements, and it does not always. The same construction in the gradient predictor compiles to a single load.

A second reader, working on a bit position with a 56-bit refill and a first table level that is always 12 bits wide, was written but not put in: coded rows were at or below parity without it.

## How the pieces map

- `shim/magicyuv_rs.c` makes three calls per packet, acting in between as the C decoder does: `header` (then it sets the pixel format and calls `ff_set_dimensions`), `tables` (then it gets the frame buffer), `decode`. After decoding it swaps the first two plane pointers for RGB formats, or sets the colour space and range, as the C code does. The format and size are set on the context even when the packet is refused later, as in C.
- `vlc.rs` is `huff_build`, `ff_vlc_init_multi_from_lengths`, `build_table`, `vlc_multi_gen` and `add_level`.
- `bits.rs` is the cached bit reader.
- `pred.rs` has the predictors, portable and AVX2. The one `unsafe` call of the AVX2 decoder is in the FFI module behind a CPU check; `magicyuv_rs_set_simd(0)` turns it off, and a quarter of the fuzz cases do.

## C behaviour that had to be copied

**A frame comes out even when slices fail.** The C decoder runs the slices through `execute2` and ignores what they return. So a slice that fails leaves its rows as they are and the frame is output anyway. A slice fails when a raw slice is shorter than its rows, or when bits that match no code turn up in the part of a row read through the joint table. From that point the rest of that plane, the other planes of the slice, and for RGB the adding of green, are skipped for that slice. Predictors are not run on the plane that failed.

**A slice that runs out of bits is not an error.** Reading stops, the rest of the rows keep what the frame buffer held, and the predictor runs over all rows. The frame buffer comes from FFmpeg's pool: zeros the first time, the contents of an older frame of the same stream after that. The port gets identical output because it gets its frames from the same pool in the same order and writes exactly the same samples. For RGB this includes swapping the plane pointers the same way, since the pool hands back the buffers by plane.

**What the bit reader returns at the end of a slice.** The reader fills a 64-bit cache, 8 bytes at first and 4 at a time after that, and refuses a fill only when its read pointer has reached the end of the slice. A slice of 1 to 8 bytes is therefore read as 8 bytes and a longer one is rounded up to a multiple of 4. The extra bytes are the start of the next slice, and their bits are decoded. A symbol is started whenever at least one bit of the slice is left, so the last symbol of a short slice can be made of bits from the next one. After the last fill the reader returns zeros. The straight port copies the reader call for call, including the counter of valid bits that wraps around when more bits are skipped than were there.

**The joint table stores 8 bytes.** `get_vlc_multi` copies a whole table entry to the row: up to six symbols (three for 16-bit samples), then the code length and the symbol count. The bytes after the symbols are normally overwritten by the next symbols. When the slice has no bits left they stay, and the predictor runs over them.

**Bits that match no code.** Code lengths that do not use the whole code space are accepted, so tables can have holes. In the last 7 samples of a row (3 for 16-bit), read one symbol at a time, a hole gives sample -1 (255 or 65535) and consumes no bits, so the rest of the row and of the slice fills up with it. Earlier in the row, read through the joint table, a hole stores one -1 and ends the slice with an error.

**Which code lengths are accepted.** Codes are handed out longest first, counting upwards. The builder refuses a set when a code would not start on a boundary of its own length (so 253 codes of 10 bits followed by codes of 2 bits are refused, while 252 of 10, 2 of 9 and 2 of 2 are accepted) and when the code space is used up before the last symbol. Lengths go up to 32; codes longer than 12 bits go through up to two levels of sub-tables.

**The joint table builder.** `add_level` tries every code twice and also tries the code just below the usable ones. Both are kept. Neither changes the table: see the last broken variant below.

**Unknown predictor.** A slice with a predictor byte other than 1, 2 or 3 is decoded and left as residuals, with a log message.

**16-bit median.** The gradient `left + top - topleft` is masked to 8 bits in the 8-bit predictor but not masked in the 16-bit one, where it can be negative or above the maximum. The first two rows of an interlaced slice both use the left predictor from zero.

## Is anything wrong in the C decoder?

Nothing was found that changes the output for valid streams.

- **Output depends on earlier frame buffer contents.** As described above, for a packet with a short or failed slice the samples that come out are whatever the buffer held, with a predictor run over them. With FFmpeg's own buffer pool that is zeros or an older picture of the same stream, not uninitialised memory. A caller that supplies its own buffers (`get_buffer2`) gets back whatever was in them. The decoder returns success. This is how the code reads and what the fuzzer exercises on every damaged packet (the port's unit test `slice_ends_early` shows it), but it was not reproduced with a standalone program against the C decoder.
- **Possible integer overflows, not reached.** `bps * width * height` in the size check of raw 16-bit slices is computed in `int` and overflows for pictures of about 150 million samples and up. The 16-bit gradient predictor adds up a row in an `int` without masking, which can overflow only for rows of tens of thousands of samples filled with out-of-range values. The port computes both without overflow. UBSan reported nothing, but the fuzzer's pictures are small.
- **A suspicion that was wrong.** Reading `add_level`, it looked as if a long code that `build_table` had cut down to its last bits could be entered into the joint table as if it were a short code, overwriting real entries, so that valid streams with codes above 12 bits would decode wrongly in the first part of each row. A test program fed the C decoder a stream built for this (code lengths 1 to 6, six of 13, 244 of 14; a row of 16 samples all coded as the 1-bit symbol). The output was correct. The loop ends at the longest short code before it gets to the cut-down one.

## Broken variants

`results/magicyuv-mutants.txt`. Five of six caught by 10 seconds of differential fuzz, each within 81 cases; three by the unit tests; one by FFmpeg's tests.

| Variant | Unit tests | FFmpeg's tests | Fuzz |
|---|---|---|---|
| Joint entry stores only its symbols, not 8 bytes | caught | missed | caught |
| No bits taken from past the end of the slice | missed | missed | caught |
| Unused code gives symbol 0 instead of -1 | caught | missed | caught |
| AVX2 left predictor: low half's total not carried to the high half | missed | caught | caught |
| 16-bit median: gradient masked like the 8-bit one | caught | missed | caught |
| Joint table: the code just below the usable ones not tried | missed | missed | missed |

The last one is not a gap. The code below the usable ones is either a short code too long to pair with the shortest code, or a long code cut down by `build_table`. In the first case it leaves no room for a second symbol, so no entry is written. In the second the loop has already ended at the longest short code. The variant builds the same tables.

The AVX2 variant is missed by the unit tests of the decoder crate because the comparison of the AVX2 and portable predictors lives in the FFI crate, where the `unsafe` call is allowed, and `mutants.sh` runs only the decoder crate's tests.

The unit test for bits past the slice misses its variant: the byte that follows the slice in that test is zero, which is also what the variant reads. The fuzzer catches it.

## The harness

`harness/codec_magicyuv.c` builds whole packets: it draws residuals, chooses code lengths in one of six ways (Huffman code for the residuals, the same limited to 12 bits, flat, Huffman code for random weights, a few heavy symbols so that there are short and very long codes, one code of every length up to 32), derives the codes as the decoder does and writes the slices. For fuzzing it can make each step wrong: lengths changed, slices cut short or too long, raw slices, unknown predictors, bad header fields, overlapping slices. All 17 formats are generated.

Mutation alone was not enough for the code lengths: changing one length nearly always makes the set invalid, and in small packets the lengths are most of the bytes. So for generated packets the harness puts the header and lengths back after mutation half of the time. With that, 64% of generated packets produce a frame.

The fuzzer also calls the Rust entry points directly to count why packets are refused; the counts are in the result lines. Every refusal reason was counted at least 1,000 times in the long run except "invalid number of slices" (about 130). That one is counted only in the direct call: it needs a height that `ff_set_dimensions` refuses first, so through FFmpeg neither decoder can reach it.

Seed files: `seed-mix-real.avi` (the seven FATE samples, one frame each), `seed-mix-enc.avi` and `seed-mix-small.avi` (twelve small clips from FFmpeg's encoder: every pixel format and predictor it has, one to four slices, sizes down to 9×7). A packet carries its own format and size, so one file can hold several.

Benchmark clips are from FFmpeg's encoder, which writes 8-bit formats with codes of at most 12 bits and one slice unless told otherwise. The 10 and 12-bit rows, the raw rows and the long-code row are generated streams. The real samples are a single 208×232 frame each, so their rows time mostly table building.

## Not ported, not covered, not verified

- **Slice threads.** The C decoder can run slices in parallel. The port does not and does not declare it. It declares frame threading, which needs nothing from the decoder (no state is kept between packets), but no check here runs with more than one thread.
- **Packet padding.** Past the end of the packet the port reads zeros. The C decoder reads the padding bytes FFmpeg requires after every packet (up to 7 of them). The harness, like `av_new_packet`, zeroes them. With non-zero padding the two would differ on slices that end at the end of the packet.
- **Large pictures.** The fuzzer's pictures are at most 250×250, plus whatever mutation makes of the size fields. The overflow cases above are out of its reach.
- **10, 12 and 14 bits.** No sample and no encoder in FFmpeg. The generator was written from the decoder source, so these formats are tested for agreement with the C decoder, not against any independent stream.
- **Sub-table position above 32767** ("strange codes" in `build_table`): ported, but whether the fuzzer reached it was not measured.
- **Direct calls.** The harness does not decode through the Rust entry point into an exact-size buffer, as some other ports do, because the output can depend on the previous contents of the frame.
- **Log messages** differ slightly from the originals; return codes and frames do not.
- **The joint-table test program** mentioned above was run once and not kept.
- Versions 3 and 4 were not saved as source; version 4's numbers are from single runs of three paths.
