# flac: FLAC audio

C source: `libavcodec/flacdec.c` (833 lines), `flac.c`, `flacdata.c`, `flacdsp.c` and its templates, and the x86 assembly `x86/flacdsp.asm`. The port registers as `flac_rs`.

The worker that ported this decoder was interrupted before writing its notes. What follows is only what the independent verification run established (main build tree, idle machine); the account of the versions, of the C behaviour that had to be copied, and of the bugs along the way is still to be written.

## Summary

- **FFmpeg's tests:** 36 of 36 pass with the port as the default decoder (`results/flac-fate.txt`). The first build of the straight port passed 13 of 36 (`results/flac-fate-first-build.txt`); why has not been written up.
- **Differential fuzz:** 151 million packets, no mismatch, no panic (`results/flac-fuzz.txt`). With overflow checks on: 20 million more, clean.
- **Sanitizer fuzz:** 17.8 million packets, clean (`results/flac-fuzz-asan.txt`).
- **Benchmark, Rust/C time, one thread, C with its assembly:** clips 0.42 to 0.92 (19 clips), single paths 0.52 to 0.88 (18 paths) (`results/flac-bench-final.txt`). The straight port was 1.13 to 3.39 on clips (`results/flac-bench-v1.txt`, measured under load).
- **Versions:** 3 (`results/flac-lib-v1-straight-port`, `-v2-fast-rice`, `-v3-final`).
- **Broken variants:** 6. The differential fuzz caught 6, the unit tests 4, FFmpeg's tests 2 (`results/flac-mutants.txt`).

## Open

- `results/flac-c-null-call.txt` records the C decoder dying with a segmentation fault when the harness presets a floating-point sample format on the context before opening it, where the port returns an error. This has not been examined or reproduced outside the worker's run.
- Everything a write-up normally covers: which decoder options and context fields the fuzzer exercises, what is not ported, what is not verified.
