/*
 * Shared between the differential fuzzer and the benchmark: the random
 * source, packet buffer, frame comparison, and the per-codec hooks.
 *
 * Adding a decoder to the harness means writing one codec_NAME.c that
 * fills in a CodecOps and listing it in common.c.
 */

#ifndef FFSAFE_HARNESS_COMMON_H
#define FFSAFE_HARNESS_COMMON_H

#include <stddef.h>
#include <stdint.h>

#include "libavcodec/avcodec.h"

#define MAX_PKT (1 << 22)

typedef struct Buf {
    uint8_t d[MAX_PKT];
    int n;
} Buf;

static inline void put(Buf *b, int byte)
{
    if (b->n < MAX_PKT)
        b->d[b->n++] = (uint8_t)byte;
}

/* What a decoder is opened with when there is no container to ask. */
typedef struct Cfg {
    int w, h;
    int bits;                   /* bits_per_coded_sample */
    uint32_t tag;               /* codec_tag */
    uint8_t extradata[1024];
    int extradata_size;
    int sample_rate, channels;  /* audio */
    /* Codec-specific choices: made by options(), applied by setup(). All
     * zero must mean the defaults. */
    int opt[4];
} Cfg;

typedef struct CodecOps {
    const char *name;           /* how the harness refers to the codec */
    const char *c_name;         /* FFmpeg's C decoder */
    const char *rs_name;        /* the Rust port */
    enum AVCodecID id;

    /* Byte values that mean something in this bitstream, for the mutator. */
    const uint8_t *interesting;
    int nb_interesting;
    /* Attach a random palette to some packets. */
    int palette_side_data;

    /* Fuzzing: cfg->w and cfg->h are set; choose the rest (or change them). */
    void (*config)(Cfg *cfg);
    /* Fuzzing: a mostly valid packet for cfg. */
    void (*gen)(Buf *b, const Cfg *cfg);

    /* Optional. Fuzzing: make the codec-specific choices of a case
     * (cfg->opt). Called for generated streams after config(), and for
     * seed files after their parameters were read. */
    void (*options)(Cfg *cfg);
    /* Optional: apply cfg to a decoder context before it is opened, for
     * AVCodecContext fields and private options beyond the basics. Both
     * decoders get the same call. */
    void (*setup)(AVCodecContext *ctx, const Cfg *cfg);
    /* Optional. Fuzzing: called on every packet after mutation, to repair
     * checksums and the like so that mutated packets get past the first
     * check. It should leave some packets unrepaired. */
    void (*fixup)(Buf *b, const Cfg *cfg);

    /* Optional: call the Rust entry point directly as well, outside
     * libavcodec. direct_step returns a status index (0 .. nb_status - 1)
     * or -1 after writing the reason to why. */
    void *(*direct_open)(const Cfg *cfg);
    int (*direct_step)(void *st, const Buf *b, const AVFrame *rs_frame,
                       int rs_got_frame, char *why, size_t why_size);
    void (*direct_close)(void *st);
    const char *const *status_names;
    int nb_status;

    /* Benchmark: space-separated names of the single-path streams, and a
     * function building one packet of a kind. cfg->w and cfg->h are set;
     * the function sets the rest. */
    const char *synth_kinds;
    AVPacket *(*synth)(const char *kind, Cfg *cfg);
} CodecOps;

/* Weak, so the harness links with whichever codec_NAME.c files exist. */
extern const CodecOps ops_rpza    __attribute__((weak));
extern const CodecOps ops_msrle   __attribute__((weak));
extern const CodecOps ops_cinepak __attribute__((weak));
extern const CodecOps ops_v210    __attribute__((weak));
extern const CodecOps ops_qtrle    __attribute__((weak));
extern const CodecOps ops_msvideo1 __attribute__((weak));
extern const CodecOps ops_targa    __attribute__((weak));
extern const CodecOps ops_vble     __attribute__((weak));
extern const CodecOps ops_loco     __attribute__((weak));
extern const CodecOps ops_magicyuv __attribute__((weak));
extern const CodecOps ops_prores   __attribute__((weak));
extern const CodecOps ops_aic      __attribute__((weak));
extern const CodecOps ops_flac     __attribute__((weak));
extern const CodecOps ops_tta      __attribute__((weak));

const CodecOps *find_codec(const char *name);

extern uint64_t rng;
uint64_t rnd64(void);
uint32_t rnd(uint32_t n);

void mutate(Buf *b, const CodecOps *ops);

void cfg_to_context(const Cfg *cfg, AVCodecContext *ctx);

/* Compare everything a caller can see. Video: format, size, flags, picture
 * type, colour properties, every visible sample of every plane, and the
 * palette. Audio: sample format, sample count, sample rate, channel
 * layout, flags, and every sample. Returns NULL when equal, else the name
 * of the first thing that differs. */
const char *frame_diff(const AVFrame *a, const AVFrame *b);

/* FNV-1a over the visible samples of every plane (and the palette), or
 * over every audio sample. */
uint64_t frame_hash(const AVFrame *f, uint64_t h);

#endif
