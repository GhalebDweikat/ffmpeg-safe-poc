# LOCO

Port of `ffmpeg/libavcodec/loco.c` (354 lines) and of the parts of `get_bits.h` and `golomb.h` it uses (the checked bit reader, `get_ur_golomb_jpegls`). Registered as `loco_rs`.

## Summary

- **FFmpeg's tests:** 2 of 2 pass with the port as the default decoder (`fate-loco-rgb`, `fate-loco-yuy2`; `results/loco-fate.txt`). They passed on the first build of the straight port too.
- **Differential fuzz:** 7,088,763 packets in 5 processes of 300 s (3 on generated streams, 2 mutating the real samples), 4,334,175 of them decoded to a frame, 0 mismatches, 0 panics (`results/loco-fuzz.txt`). No fuzz run at any point found a mismatch.
- **Sanitizer fuzz** (AddressSanitizer + UBSan on the C code): 827,943 packets, clean (`results/loco-fuzz-asan.txt`). Clean only because the fuzzer is kept away from one undefined behaviour in the C decoder, described below.
- **Benchmark, Rust time / C time** (median of 31 passes; the machine was never idle, see the load averages in the files):
  - straight port: clips 1.24 and 1.29, paths 1.28 to 1.54 (`results/loco-bench-v1.txt`)
  - final: clips 0.94 (RGB sample) and 1.10 (YUY2 sample; 0.91 by best pass), paths 0.63 to 0.94 (`results/loco-bench-final.txt`, load average 7 to 8)
  - the same final binary measured while the load average rose from 4 to 14: clips 1.11 and 1.05, paths 0.63 to 1.19 (`results/loco-bench-final-under-load.txt`). The ratio moves by up to 0.2 with the load, so the YUY2 clip and the `noise` path are "at parity", not "faster", until measured on an idle machine.
- **Versions:** 6 (the straight port and five more; the fifth was slower on coded pixels than the fourth and was half undone in the sixth).
- **Bugs in the port:** none reached a build that was tested. One was written and caught reading the code back before compiling: in the v4 restructure the per-pixel loop read a pixel's code before checking that the row had a pixel left, which would have eaten the first code of the next row.
- **Bugs in my harness:** the first broken-variant run showed a generator gap: a decoder that does not treat a code value of all ones as an error was caught by nothing, because the generator stopped writing the plane after such a code, so the broken decoder failed a little later anyway. The generator now continues the stream; the second run catches all 6 variants (`results/loco-mutants.txt`; the first run is `results/loco-mutants-before-generator-fix.txt`). FFmpeg's tests catch 1 of 6, the unit tests 4 of 6, 10 s of fuzz 6 of 6.
- **C behaviour copied:** a packet is good only if every code lies wholly inside it (the C reader reads zero padding past the end and goes on, but then always rejects the packet at the end of the plane); every error is `AVERROR(EINVAL)`; code values wrap at 32 bits, and a wrapped value of all ones is an error; a plane must leave one byte for the next; chroma planes of odd-sized pictures are coded with the size rounded down, so their last column or row is never written (it stays as the buffer was, zero with FFmpeg's pool), while a plane of coded width 0 still gets one pixel per row; odd-width RGB (not RGBA) is sheared back after decoding; the running sum wraps.
- **Wrong in the C decoder:** signed integer overflow (undefined behaviour) at `loco.c:75`, `r->sum += val`, reachable with a packet of about one megabyte of zero bytes on a picture of three pixels. Reproduce: `docker exec -e LOCO_UB=1 ffsafe /build/w/loco/harness/difffuzz-asan loco 77 60` prints `libavcodec/loco.c:75:12: runtime error: signed integer overflow: 1176151671 + 971335195 cannot be represented in type 'int'` (`results/loco-ub-sanitizer.txt`). With gcc 12 the sum just wraps; no out-of-bounds access follows. Not reported upstream.
- **Not ported / not covered:** see the last section. In short: nothing of the decoder is left out; packets over 4 MB, the `INT_MIN` residual check (reachable only through the overflow above) and callers that supply their own frame buffers are not covered by the fuzzer.
- **Changes needed in shared files:** none. Nothing in the shared harness or scripts blocked this port.
- **Not verified:** the final benchmark on an idle machine; the long fuzz and the sanitizer fuzz were run before the generator fix and not repeated after it (after it: a 20 s gate, 455,371 packets, clean, on the normal build only); that the port's wrapping sum agrees with the compiled C was checked by one 40 s run with `LOCO_UB=1` on the normal build (206,825 packets, no mismatch; `results/loco-ub-plain-build.txt`), without confirming how many of those packets overflowed.

## Files

| File | Lines | What |
|---|---|---|
| `rust/loco-core/src/lib.rs` | 998, of which about 490 are code outside the tests | the decoder |
| `rust/loco-core/src/reference.rs` | 224 | test-only: the straight port of the plane decoder, which the unit tests compare the optimised one against |
| `rust/ffsafe-ffi/src/loco.rs` | 143 | C entry points `loco_rs_init`, `loco_rs_decode` |
| `shim/loco_rs.c` | 142 | FFmpeg glue |
| `harness/codec_loco.c` | 658 | LOCO encoder (Golomb-Rice writer with the decoder's state machine), fuzz generator, a model of the C decoder's arithmetic, benchmark pictures |
| `scripts/codecs/loco.sh` | | tests, samples, broken variants; sets `SYNTH_FRAMES=12` |

The decoder keeps nothing between frames. The shim holds the two numbers the extradata yields (pixel format, lossy parameter) and passes them to every decode call, so there is no heap object on the Rust side.

## How the C decoder behaves, and what the port copies

1. **Reads past the end of the packet.** The C bit reader is the checked one: its position is clamped to `size + 8` bits, and a 32-bit load at that position reads up to five bytes of FFmpeg's zero padding. `get_ur_golomb_jpegls` can therefore return a value whose last bits came from the padding, and `loco_get_rice` goes on with it. But the position never goes back, and `loco_decode_plane` returns `(position + 7) / 8`, which is then `size + 1`; `decode_frame` rejects that for every plane (`decoded >= buf_size` for all but the last, `decoded > buf_size` for the last). So the C decoder outputs a frame only if every code lies wholly inside the packet, and what the padding holds never reaches a caller. The straight port copied the clamp and the padding reads literally; the final version reads zeros past the end, remembers that it did, and rejects the plane at its end. The unit test `agrees_with_the_straight_port` and the differential fuzz check that the two rules accept the same packets.
2. **Every error is `AVERROR(EINVAL)`**, whatever `loco_decode_plane` returned, and a frame is output only if all planes decoded. The frame buffer is requested before the packet is looked at, so a rejected packet still costs a buffer; the port does the same.
3. **Code values wrap at 32 bits.** A code is `z` zero bits, a one, `k` bits, with value `(z << k) + bits` computed in `unsigned`. With `k = 9`, 2^23 zero bits (one megabyte) wrap it. A wrapped value of all ones equals the reader's error value -1 and is an error; a mapped residual equal to `INT_MIN` is one too. Both are copied.
4. **A non-final plane must leave at least one byte** for the next (`decoded >= buf_size`), even if the next plane would be rejected anyway.
5. **Chroma planes of odd-sized pictures.** They are coded with the size rounded down (`width / 2`, `height / 2`), while the frame's chroma planes are rounded up. The last chroma column (4:2:2 and 4:2:0, odd width) and the last chroma row (4:2:0, odd height) are never written: a caller sees whatever the buffer held, which with libavcodec's own buffer pool is zero. A plane coded with width 0 (picture width 1) still gets the first pixel of every row, and one coded with height 0 still gets its first row, because the C loops write those unconditionally. The port writes exactly the same samples and leaves the others alone.
6. **`rotate_faulty_loco`.** RGB (not RGBA) pictures of odd width are sheared back after decoding, on whatever was decoded. Copied.
7. **The sum of code values wraps** in the port (`wrapping_add`), which is what the compiled C does; in C it is undefined behaviour, see below.
8. Version 1 streams ignore the lossy field; any other version reads it; a lossy value outside 0..65536 and an unknown mode are refused at open. Extradata longer than 12 bytes is accepted.

## Wrong in the C decoder

**Signed integer overflow (undefined behaviour) in `loco_update_rice_param`.** `r->sum += val` adds half of every code value to an `int`. A value can be close to 2^32 (point 3 above), so a packet of a little over one megabyte overflows the sum: for example a first code that makes the Rice parameter 9, followed by about 2^23 - 1 zero bits and a one. The picture can be as small as 3 pixels. With gcc 12 on x86-64 the sum wraps to a negative number and the parameter becomes 0; nothing is read or written out of bounds.

Reproduce, in the sanitizer build that `asan.sh loco` makes:

```
docker exec -e LOCO_UB=1 ffsafe /build/w/loco/harness/difffuzz-asan loco 77 60
libavcodec/loco.c:75:12: runtime error: signed integer overflow: 1176151671 + 971335195 cannot be represented in type 'int'
```

(`results/loco-ub-sanitizer.txt`; seed 78 gives the same report with other numbers.) In the normal build the same generator setting ran 206,825 packets without a mismatch (`results/loco-ub-plain-build.txt`), so the port's wrapping addition agrees with what gcc compiled, as far as that one run shows.

The fuzzer stays away from exactly this: the generator knows the sum and does not write a code that would overflow it, and `fixup` follows mutated packets of a megabyte or more with a model of the C arithmetic (`find_overflow` in `harness/codec_loco.c`) and cuts them off just before the overflowing code. Smaller packets cannot overflow (each code adds at most 257 times its length in bits). `LOCO_UB=1` turns both off.

The same kind of overflow exists for `r->save += r->run + 1` (needs a 64 MB run code) and is not reachable by the fuzzer's 4 MB packets. Not reported upstream.

Point 5 (chroma of odd-sized pictures left unwritten) is probably also not what the format intends, but there is no specification and no sample to check it against.

## Versions

| Version | What changed | Rust/C, clips | Rust/C, paths |
|---|---|---|---|
| v1 straight port | the C, statement by statement | 1.24, 1.29 | 1.28 to 1.54 |
| v2 | 64-bit bit window, zero runs as blocks, rows as slices | 1.07, 1.35 (loaded machine) | 0.87 to 1.19 (loaded) |
| v3 | per-pixel loop in its own function, slow paths by value | not measured as a full table | a few rows: 0.89 to 1.23 (loaded) |
| v4 | state in registers | a few passes: 1.03, 1.11 (loaded; 0.87, 0.90 by best pass) | a few rows: 0.89 to 1.08 (loaded) |
| v5 | prediction by cases | 1.19, 1.24 (load rising) | 0.65 to 1.18 |
| v6 final | v4 + prediction by cases in zero runs only | 0.94, 1.10 | 0.63 to 0.94 |

Notes on the versions:

- **v1 straight port** (`results/loco-lib-v1-straight-port.rs`): the C reader literally, a 32-bit load per code.
- **v2** (`results/loco-lib-v2-window-and-runs/`): a 64-bit bit window refilled eight bytes at a time; zero runs done as a block (the Rice state is advanced by n pixels at once, the first row is filled); rows as slices.
- **v3**: the rarely used reader paths take and return the reader by value; the per-pixel loop moved into a function of its own. The disassembly still showed the bit window being stored and loaded on the stack for every pixel.
- **v4**: no tentative copies of the window (a second stop reason instead), the run check out of the per-pixel loop, iterators instead of indices. The window, its bit count, the sum, the count and the neighbouring pixels are now in registers.
- **v5**: the prediction written by cases (two comparisons on the left pixel instead of a chain of four). Faster in zero runs, about 10% slower on coded pixels in a back-to-back comparison with v4, where the bit reader, not the prediction, is what a pixel waits for. Not kept as it was.
- **v6 final**: v4, with the prediction by cases only inside zero runs.

What was not tried: a closed form for the Rice parameter instead of the C loop; BMI2/LZCNT code behind a CPU check (the build targets plain x86-64, so `leading_zeros` is `bsr` and shifts go through `cl`, as in the C).

## Benchmarks

The machine was shared with other agents' fuzz runs for most of the work, and the ratio moved with the load: with 10 to 18 other busy processes the same binary measured up to 0.2 higher than in a quiet minute. `results/loco-bench-final.txt` and the other tables have the load average printed where I recorded it. The only real clips are the two FATE samples (188x128, 5 and 3 frames, about 0.4 ms per frame), so the clip rows are short measurements.

The benchmark paths are pictures encoded by the harness: `flat` (one value: all zero runs), `screen` (flat rectangles, some with a two-colour pattern), `clean` (smooth shapes, noise of ±1), `photo` (smooth shapes, noise of ±9, about 4.8 bits per pixel; the real samples have about 6), `noise` (random bytes). `photo-rgbodd` is 1279 pixels wide (the table says 1280), which adds the shear.

## Harness

- `config` picks version, mode (all nine, and some unknown ones) and lossy value (0, small, around 65536, random), and sometimes short or long extradata.
- `gen` writes valid streams of random code values, not encoded pictures: per plane one of eight value distributions, zero runs at full length, cut short, or at random lengths; random padding bits; sometimes a packet that ends after an earlier plane, or has bytes after the last one. One plane in 1000 gets a megabyte code (value wraps to a small number, or to all ones).
- `fixup`: see above.
- `synth`: encodes generated pictures, for the benchmark.

## Not ported, not covered

- Nothing of `loco.c` is left out. The port is single-threaded, as the C decoder is.
- Packets larger than 4 MB (the harness limit). The refusal of buffers of 256 MB and more (`init_get_bits8`) is copied but never run.
- The `INT_MIN` residual check and the wrapping of the sum are reachable only together with the C decoder's undefined behaviour, so the normal fuzz runs never get there; see the one `LOCO_UB=1` run above.
- A caller that supplies its own frame buffers (`get_buffer2`): the never-written chroma samples of odd-sized pictures would then show the caller's bytes in both decoders, which was not run. The shim refuses a negative line size with `AVERROR_BUG`, where the C decoder would decode upside down; also not run.
- Packet padding that is not zero. The argument in point 1 says a caller cannot see a difference; it was not tested, since the harness always pads with zeros.
- The Rust entry point is not also called directly on exact-size buffers (the harness's optional `direct_*` hooks are not implemented).
- The long fuzz and the sanitizer fuzz ran with the generator as it was before the fix described in the summary (megabyte codes in 1 plane of 3000 instead of 1000, and the stream stopping after an all-ones code).
- Only two real clips exist, both 188x128 and under a second long. There is no real 4:2:0 or RGBA sample; those modes are covered by generated streams only.
- Benchmarks: Docker on WSL2 on a hybrid-core machine shared with other fuzz runs, pinned to one virtual CPU. The final table needs repeating on an idle machine.
