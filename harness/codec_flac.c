/* FLAC: packet generators.
 *
 * For the fuzzer: frames that are well-formed on the outside (sync code,
 * header checksum, subframe headers, partition structure) and random on
 * the inside, plus the things a FLAC decoder is handed that are not
 * frames (file headers, Ogg header packets, Vorbis comments).
 *
 * For the benchmark: a small real encoder. It codes a made-up signal with
 * a chosen subframe type, predictor order and stereo mode, so that each
 * stream exercises one decoder path at the cost it has in real files.
 */

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "config.h"

#include "libavutil/cpu.h"
#include "libavutil/opt.h"
#include "libavutil/samplefmt.h"

#include "common.h"

/* Test hook in the Rust library: 0 forces the portable code paths. */
void flac_rs_set_simd(int enabled);

/* ---- checksums ---- */

static uint8_t crc8(const uint8_t *d, int n)
{
    uint8_t c = 0;
    for (int i = 0; i < n; i++) {
        c ^= d[i];
        for (int k = 0; k < 8; k++)
            c = c & 0x80 ? (c << 1) ^ 0x07 : c << 1;
    }
    return c;
}

static uint16_t crc16_table[256];

static uint16_t crc16(const uint8_t *d, int n)
{
    uint16_t c = 0;
    if (!crc16_table[1])
        for (int i = 0; i < 256; i++) {
            uint16_t v = i << 8;
            for (int k = 0; k < 8; k++)
                v = v & 0x8000 ? (v << 1) ^ 0x8005 : v << 1;
            crc16_table[i] = v;
        }
    for (int i = 0; i < n; i++)
        c = (c << 8) ^ crc16_table[(c >> 8) ^ d[i]];
    return c;
}

/* ---- bit writer ---- */

typedef struct BW {
    Buf *b;
    uint64_t acc;
    int n;                      /* bits waiting in acc, fewer than 8 */
} BW;

static void bw_put(BW *w, int n, uint64_t v)     /* up to 32 bits */
{
    if (!n)
        return;
    w->acc = (w->acc << n) | (v & ((1ULL << n) - 1));
    w->n  += n;
    while (w->n >= 8) {
        put(w->b, (int)((w->acc >> (w->n - 8)) & 0xff));
        w->n -= 8;
    }
}

static void bw_put64(BW *w, int n, uint64_t v)
{
    if (n > 32) {
        bw_put(w, n - 32, v >> 32);
        n = 32;
    }
    bw_put(w, n, v);
}

static void bw_zeros(BW *w, unsigned n)
{
    for (; n >= 32; n -= 32)
        bw_put(w, 32, 0);
    bw_put(w, n, 0);
}

static void bw_align(BW *w)
{
    if (w->n)
        bw_put(w, 8 - w->n, 0);
}

static void bw_rice(BW *w, int k, uint32_t folded)
{
    bw_zeros(w, folded >> k);
    bw_put(w, 1, 1);
    bw_put(w, k, folded);
}

/* ---- frame header ---- */

static const int blocksize_table[16] = {
    0, 192, 576, 1152, 2304, 4608, 0, 0, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768
};

static int bps_code_of(int bps)
{
    switch (bps) {
    case 8:  return 1;
    case 12: return 2;
    case 16: return 4;
    case 20: return 5;
    case 24: return 6;
    case 32: return 7;
    }
    return 0;                   /* "as in STREAMINFO" */
}

/* Writes the header with its checksum. bs_code -1: pick one for
 * blocksize. number: the coded frame number, nb bytes. */
static void frame_header(BW *w, int blocksize, int bs_code, int sr_code, int sr_extra,
                         int assignment, int bps_code, const uint8_t *number, int nb)
{
    int start = w->b->n;

    if (bs_code < 0) {
        bs_code = blocksize <= 256 ? 6 : 7;
        for (int i = 1; i < 16; i++)
            if (blocksize_table[i] == blocksize)
                bs_code = i;
    }
    bw_put(w, 16, 0xfff8);
    bw_put(w, 4, bs_code);
    bw_put(w, 4, sr_code);
    bw_put(w, 4, assignment);
    bw_put(w, 3, bps_code);
    bw_put(w, 1, 0);
    for (int i = 0; i < nb; i++)
        bw_put(w, 8, number[i]);
    if (bs_code == 6)
        bw_put(w, 8, blocksize - 1);
    else if (bs_code == 7)
        bw_put(w, 16, blocksize - 1);
    if (sr_code == 12)
        bw_put(w, 8, sr_extra);
    else if (sr_code == 13 || sr_code == 14)
        bw_put(w, 16, sr_extra);
    bw_put(w, 8, crc8(w->b->d + start, w->b->n - start));
}

static void frame_footer(BW *w, int start)
{
    bw_align(w);
    bw_put(w, 16, crc16(w->b->d + start, w->b->n - start));
}

static void streaminfo(uint8_t *p, int max_blocksize, int samplerate, int channels, int bps)
{
    uint32_t v = ((uint32_t)samplerate << 12) | ((channels - 1) << 9) | ((bps - 1) << 4);

    memset(p, 0, 34);
    p[0]  = p[2] = max_blocksize >> 8;
    p[1]  = p[3] = max_blocksize;
    p[10] = v >> 24;
    p[11] = v >> 16;
    p[12] = v >> 8;
    p[13] = v;
    for (int i = 18; i < 34; i++)
        p[i] = rnd(256);
}

/* ---- fuzzing ---- */

/* What the stream of the current case looks like; chosen in config(). */
static struct {
    int channels;
    int bps;
    int blocksize;
    int have_info;              /* the extradata holds a usable STREAMINFO */
    int packets;
} st;

static const int some_bps[] = { 8, 12, 16, 16, 16, 20, 24, 24, 32, 32 };
static const int some_blocksizes[] = {
    16, 16, 17, 20, 32, 32, 48, 64, 64, 96, 100, 128, 192, 192, 256, 576, 1000, 1024, 1152, 4096,
};

static void config(Cfg *cfg)
{
    int info_bps, max_blocksize;
    uint8_t *e = cfg->extradata;

    memset(&st, 0, sizeof(st));
    st.channels  = rnd(3) ? 1 + rnd(2) : 1 + rnd(8);
    st.bps       = some_bps[rnd(FF_ARRAY_ELEMS(some_bps))];
    st.blocksize = rnd(4) ? some_blocksizes[rnd(FF_ARRAY_ELEMS(some_blocksizes))] : 1 + rnd(300);
    if (rnd(300) == 0)
        st.blocksize = 65535 - rnd(3) * 30000;

    /* The context's own idea of the stream, as a demuxer would set it. */
    cfg->w = cfg->h = 0;
    cfg->sample_rate = rnd(3) ? 0 : 8000 << rnd(4);
    cfg->channels    = rnd(3) ? 0 : rnd(4) ? st.channels : 1 + rnd(8);

    /* STREAMINFO can hold sample sizes a frame header cannot. */
    info_bps      = rnd(8) ? st.bps : 4 + rnd(29);
    max_blocksize = rnd(6) ? FFMAX(st.blocksize, 16) + rnd(3) * rnd(5000) : 16 + rnd(65520);
    if (max_blocksize > 65535)
        max_blocksize = 65535;
    switch (rnd(10)) {
    case 0: case 1: case 2: case 3:             /* no extradata */
        break;
    case 4: case 5: case 6:                     /* the bare block */
        streaminfo(e, max_blocksize, rnd(4) ? 44100 : rnd(3) * rnd(655350),
                   rnd(6) ? st.channels : 1 + rnd(8), info_bps);
        cfg->extradata_size = 34 + (rnd(8) ? 0 : 1 + rnd(20));
        st.have_info = 1;
        st.bps       = info_bps;
        break;
    case 7: case 8:                             /* a file header */
        memcpy(e, "fLaC", 4);
        e[4] = rnd(4) ? 0x80 : rnd(256);
        e[5] = e[6] = 0;
        e[7] = 34;
        streaminfo(e + 8, max_blocksize, rnd(4) ? 48000 : rnd(655350),
                   rnd(6) ? st.channels : 1 + rnd(8), info_bps);
        cfg->extradata_size = 42 + (rnd(8) ? 0 : 1 + rnd(20));
        st.have_info = 1;
        st.bps       = info_bps;
        break;
    default:                                    /* broken in some way */
        streaminfo(e, max_blocksize, 44100, st.channels, st.bps);
        switch (rnd(5)) {
        case 0: cfg->extradata_size = 1 + rnd(33);                      break;
        case 1: e[2] = 0; e[3] = rnd(16); cfg->extradata_size = 34;     break;  /* blocks < 16 */
        case 2: e[12] &= 0xfe; e[13] = rnd(3) << 4; cfg->extradata_size = 34; break; /* bps < 4 */
        case 3: memcpy(e, "fLaC", 4); cfg->extradata_size = 34 + rnd(8); break;
        default:
            for (int i = 0; i < 34; i++)
                e[i] = rnd(256);
            cfg->extradata_size = 34;
            st.have_info = 1;                   /* if it is accepted, its bps is unknown */
            st.bps       = 4 + rnd(29);
            break;
        }
        break;
    }
}

/* cfg->opt:
 *   [0] request_sample_fmt (index into req_fmts)
 *   [1] err_recognition
 *   [2] bit 0: use_buggy_lpc; the rest: a sample format the context is
 *       given before it is opened (index into preset_fmts)
 *   [3] bits 0-1: which CPU extensions FFmpeg may use; bit 2: the Rust
 *       decoder runs without its AVX2 paths
 */
static const enum AVSampleFormat req_fmts[] = {
    AV_SAMPLE_FMT_NONE, AV_SAMPLE_FMT_S16, AV_SAMPLE_FMT_S32, AV_SAMPLE_FMT_S16P,
    AV_SAMPLE_FMT_S32P, AV_SAMPLE_FMT_U8,  AV_SAMPLE_FMT_FLT, AV_SAMPLE_FMT_DBLP,
    AV_SAMPLE_FMT_S64,  AV_SAMPLE_FMT_U8P,
};
static const enum AVSampleFormat preset_fmts[] = {
    AV_SAMPLE_FMT_NONE, AV_SAMPLE_FMT_S16, AV_SAMPLE_FMT_S32, AV_SAMPLE_FMT_S16P,
    AV_SAMPLE_FMT_S32P,
};

static void options(Cfg *cfg)
{
    static const int err[] = {
        0, 0, 0, 0, AV_EF_CRCCHECK, AV_EF_CRCCHECK | AV_EF_EXPLODE, AV_EF_COMPLIANT,
        AV_EF_EXPLODE, AV_EF_COMPLIANT | AV_EF_EXPLODE, AV_EF_BITSTREAM | AV_EF_CRCCHECK,
    };
    int level = rnd(3) ? 0 : 1 + rnd(3);

    cfg->opt[0] = rnd(2) ? 0 : rnd(FF_ARRAY_ELEMS(req_fmts));
    cfg->opt[1] = err[rnd(FF_ARRAY_ELEMS(err))];
    cfg->opt[2] = (rnd(6) == 0) | ((rnd(4) ? 0 : rnd(FF_ARRAY_ELEMS(preset_fmts))) << 1);
    cfg->opt[3] = level | ((rnd(4) == 0) << 2);

    /* FFmpeg's FLAC assembly needs SSE2 (stereo, 16 bit), SSSE3 (more
     * channels) and SSE4 (prediction); without them it runs C code that
     * is not the same on damaged streams. Both decoders look at the CPU
     * flags for every frame. */
    switch (level) {
    case 0: av_force_cpu_flags(-1); break;      /* whatever the CPU has */
    case 1: av_force_cpu_flags(0);  break;
    case 2: av_force_cpu_flags(AV_CPU_FLAG_MMX | AV_CPU_FLAG_MMXEXT | AV_CPU_FLAG_SSE |
                               AV_CPU_FLAG_SSE2);
            break;
    case 3: av_force_cpu_flags(AV_CPU_FLAG_MMX | AV_CPU_FLAG_MMXEXT | AV_CPU_FLAG_SSE |
                               AV_CPU_FLAG_SSE2 | AV_CPU_FLAG_SSE3 | AV_CPU_FLAG_SSSE3);
            break;
    }
    flac_rs_set_simd(!(cfg->opt[3] & 4));
}

static void setup(AVCodecContext *ctx, const Cfg *cfg)
{
    /* The fuzzer does not print a case's configuration with a mismatch;
     * FLAC_FUZZ_VERBOSE=1 prints every decoder's as it is opened. */
    if (getenv("FLAC_FUZZ_VERBOSE")) {
        fprintf(stderr, "setup %s: opt=%d,%d,%d,%d rate=%d channels=%d extradata=%d:",
                ctx->codec->name, cfg->opt[0], cfg->opt[1], cfg->opt[2], cfg->opt[3],
                cfg->sample_rate, cfg->channels, cfg->extradata_size);
        for (int i = 0; i < cfg->extradata_size; i++)
            fprintf(stderr, "%02x", cfg->extradata[i]);
        fputc('\n', stderr);
    }
    ctx->request_sample_fmt = req_fmts[cfg->opt[0]];
    ctx->err_recognition    = cfg->opt[1];
    if (cfg->opt[2] & 1)
        av_opt_set_int(ctx->priv_data, "use_buggy_lpc", 1, 0);
    if (cfg->opt[2] >> 1)
        ctx->sample_fmt = preset_fmts[cfg->opt[2] >> 1];
    /* Not part of the fuzzing: FLAC_PRESET_FLT=flac (or flac_rs) opens
     * that decoder with a sample format it cannot write, to show the C
     * decoder's null function pointer call (see notes/flac.md). */
    if (getenv("FLAC_PRESET_FLT") && !strcmp(getenv("FLAC_PRESET_FLT"), ctx->codec->name))
        ctx->sample_fmt = AV_SAMPLE_FMT_FLT;
}

/* A sample of the given width: any bit pattern, or a small value. */
static void fuzz_sample(BW *w, int bits, int tame)
{
    uint64_t v = rnd64();
    if (tame) {
        int mag = bits > 6 ? bits - 5 : 1;
        v = (int64_t)(v & ((1ULL << mag) - 1)) - (1LL << (mag - 1));
    }
    bw_put64(w, bits, v);
}

static void fuzz_residual(BW *w, int blocksize, int order, int tame)
{
    int method  = rnd(40) == 0 ? 2 + rnd(2) : rnd(5) == 0;
    int rice_bits = 4 + (method & 1), esc = (1 << rice_bits) - 1;
    int max_po = 0, po;

    while (max_po < 15 && !(blocksize & (1 << max_po)))
        max_po++;
    if (max_po > 6)
        max_po = 6;
    po = rnd(max_po + 1);
    while (po > 0 && (blocksize >> po) < order && rnd(20))  /* else the order is refused */
        po--;
    if (rnd(60) == 0)
        po = rnd(16);
    bw_put(w, 2, method);
    bw_put(w, 4, po);

    for (int part = 0; part < 1 << po; part++) {
        int n = (blocksize >> po) - (part ? 0 : order);
        int k = tame ? rnd(4) : rnd(3) ? rnd(8) : rnd(esc);
        if (rnd(12) == 0) {                     /* raw samples */
            int bits = rnd(3) ? rnd(8) : rnd(32);
            bw_put(w, rice_bits, esc);
            bw_put(w, 5, bits);
            for (int i = 0; i < n; i++)
                bw_put(w, bits, rnd64());
            continue;
        }
        bw_put(w, rice_bits, k);
        {
            /* The decoder takes at most limit - 2 zeros in a row; limit - 1
             * are an escape with one more bit, limit an error. */
            int64_t limit = k > 1 ? (INT32_MAX >> (k - 1)) + 2 : INT32_MAX;
            int long_runs = rnd(10) == 0;
            for (int i = 0; i < n; i++) {
                int64_t q = __builtin_ctzll(rnd64() | (1ULL << 40));
                if (long_runs && rnd(8) == 0)
                    q = rnd(80);
                if (rnd(k >= 25 ? 300 : 2000) == 0)
                    q = limit - 3 + rnd(4);     /* around the limit */
                else if (q > limit - 2)
                    q = rnd(limit - 1);
                if (q > 20000)                  /* (keeps packets small) */
                    q = rnd(20000);
                bw_zeros(w, q);
                bw_put(w, 1, 1);
                if (q == limit - 1)
                    bw_put(w, 1, rnd(2));
                else
                    bw_put(w, k, rnd64());
            }
        }
    }
}

static void fuzz_subframe(BW *w, int bps, int blocksize, int tame)
{
    int type = rnd(100), order = 0, wasted = 0;

    bw_put(w, 1, rnd(200) == 0);
    if (type < 8)
        type = 0;                               /* constant */
    else if (type < 18)
        type = 1;                               /* verbatim */
    else if (type < 50)
        type = 8 + (order = rnd(5));            /* fixed */
    else if (type < 98) {                       /* LPC */
        order = rnd(3) ? 1 + rnd(8) : rnd(3) ? 1 + rnd(32) : 32;
        if (order > blocksize && rnd(10))
            order = 1 + rnd(blocksize);
        type = 32 + order - 1;
    } else
        type = rnd(64);                         /* often a reserved type */
    bw_put(w, 6, type);
    order = type >= 32 ? type - 31 : type >= 8 && type <= 12 ? type - 8 : 0;

    if (rnd(6) == 0) {
        wasted = 1 + rnd(rnd(4) ? 4 : bps + 2);
        bw_put(w, 1, 1);
        bw_zeros(w, wasted - 1);
        bw_put(w, 1, 1);
        bps -= wasted;
        if (bps < 1)
            bps = 1;                            /* (the decoder has given up) */
    } else {
        bw_put(w, 1, 0);
    }

    if (type == 0) {
        fuzz_sample(w, bps, tame);
    } else if (type == 1) {
        for (int i = 0; i < blocksize; i++)
            fuzz_sample(w, bps, tame);
    } else if (type >= 8 && type <= 12) {
        for (int i = 0; i < order; i++)
            fuzz_sample(w, bps, tame);
        fuzz_residual(w, blocksize, order, tame);
    } else if (type >= 32) {
        int prec = tame ? 2 + rnd(5) : rnd(30) ? 1 + rnd(15) : 16;
        int qlevel = tame ? prec + 2 + rnd(4) : rnd(30) ? rnd(16) : 16 + rnd(16);
        for (int i = 0; i < order; i++)
            fuzz_sample(w, bps, tame);
        bw_put(w, 4, prec - 1);
        bw_put(w, 5, qlevel > 15 && tame ? 15 : qlevel);
        for (int i = 0; i < order; i++)
            bw_put(w, prec, rnd64());
        fuzz_residual(w, blocksize, order, tame);
    }
}

static void fuzz_frame(Buf *b)
{
    static const int assignments[] = { 1, 1, 8, 9, 10, 10 };
    BW w = { b };
    uint8_t number[7];
    int start = b->n, nb = 1, tame = rnd(3) == 0;
    int channels = st.channels, assignment, bps_code, sr_code, blocksize = st.blocksize;

    if (rnd(12) == 0)
        channels = 1 + rnd(8);
    if (rnd(40) == 0)
        st.channels = channels;
    assignment = channels == 2 ? assignments[rnd(6)] : channels - 1;
    if (rnd(150) == 0)
        assignment = rnd(16);
    channels = assignment < 8 ? assignment + 1 : 2;

    bps_code = bps_code_of(st.bps);
    if (st.have_info && rnd(2))
        bps_code = 0;
    if (rnd(40) == 0)
        bps_code = rnd(8);

    sr_code = rnd(3) ? 9 : rnd(15);
    if (rnd(300) == 0)
        sr_code = 15;

    if (rnd(5) == 0)
        blocksize = rnd(3) ? some_blocksizes[rnd(FF_ARRAY_ELEMS(some_blocksizes))] : 1 + rnd(600);

    /* The frame number: one to seven bytes of the extended UTF-8 coding. */
    number[0] = rnd(128);
    if (rnd(3) == 0) {
        static const uint8_t lead[] = { 0xc0, 0xe0, 0xf0, 0xf8, 0xfc, 0xfe };
        nb = 2 + rnd(6);
        number[0] = lead[nb - 2] | rnd(nb == 7 ? 2 : 1 << (7 - nb));
        for (int i = 1; i < nb; i++)
            number[i] = 0x80 | rnd(64);
        if (rnd(30) == 0)
            number[rnd(nb)] = rnd(256);
    }

    frame_header(&w, blocksize, rnd(4) ? -1 : blocksize <= 256 && rnd(2) ? 6 : 7,
                 sr_code, rnd(65536), assignment, bps_code, number, nb);
    if (rnd(60) == 0)
        b->d[b->n - 1] ^= 1 << rnd(8);          /* a wrong header checksum */

    for (int ch = 0; ch < channels; ch++) {
        int side = (assignment == 8 && ch == 1) || (assignment == 9 && ch == 0) ||
                   (assignment == 10 && ch == 1);
        fuzz_subframe(&w, st.bps + side, blocksize, tame);
    }
    frame_footer(&w, start);
    if (rnd(12) == 0)
        b->d[b->n - 1 - rnd(2)] ^= 1 << rnd(8); /* a wrong frame checksum */
}

static void gen(Buf *b, const Cfg *cfg)
{
    int kind = rnd(100);

    b->n = 0;
    /* A file header as packet data; likelier at the start of a stream
     * without extradata, where it still counts. */
    if (kind < (st.packets == 0 && !cfg->extradata_size ? 30 : 3)) {
        uint8_t info[34];
        int blocks = rnd(3);
        memcpy(b->d, "fLaC", 4);
        b->n = 4;
        put(b, blocks || rnd(8) == 0 ? 0 : 0x80);
        put(b, 0);
        put(b, 0);
        put(b, rnd(30) ? 34 : rnd(256));
        streaminfo(info, rnd(8) ? FFMAX(st.blocksize, 16) + rnd(100) : rnd(65536),
                   rnd(4) ? 44100 : rnd(655350), rnd(4) ? st.channels : 1 + rnd(8),
                   rnd(6) ? st.bps : 1 + rnd(32));
        for (int i = 0; i < 34; i++)
            put(b, info[i]);
        for (int k = 0; k < blocks; k++) {
            int size = rnd(40);
            put(b, (k == blocks - 1 && rnd(10) ? 0x80 : 0) | (1 + rnd(6)));
            put(b, 0);
            put(b, 0);
            put(b, size);
            for (int i = 0; i < size; i++)
                put(b, rnd(256));
        }
        if (rnd(3) == 0)
            b->n -= rnd(b->n > 12 ? 12 : 1);
        if (rnd(2))
            fuzz_frame(b);
    } else if (kind < 5) {                      /* an Ogg FLAC header packet */
        memcpy(b->d, "\177FLAC\1\0\0\1fLaC", 13);
        b->n = 13 + rnd(40);
    } else if (kind < 7) {                      /* a Vorbis comment block */
        b->n = 1 + rnd(60);
        for (int i = 0; i < b->n; i++)
            b->d[i] = rnd(256);
        b->d[0] = rnd(2) ? 0x04 : 0x84;
    } else if (kind < 9) {                      /* too short to be a frame */
        fuzz_frame(b);
        b->n = 1 + rnd(9);
    } else {
        fuzz_frame(b);
        while (rnd(8) == 0 && b->n < MAX_PKT / 4)       /* several frames in a packet */
            fuzz_frame(b);
        if (rnd(20) == 0)
            for (int i = rnd(12); i > 0; i--)
                put(b, rnd(256));
    }
    st.packets++;
}

/* Length of the frame header at the start of d, without its checksum, or
 * 0 if it is not a header. */
static int header_length(const uint8_t *d, int n)
{
    int len = 5, bs_code, sr_code;

    if (n < 6 || d[0] != 0xff || (d[1] & 0xfe) != 0xf8)
        return 0;
    bs_code = d[2] >> 4;
    sr_code = d[2] & 15;
    for (int bit = 0x80; d[4] & bit; bit >>= 1)
        len++;
    if (d[4] & 0x80)
        len--;
    len += bs_code == 6 ? 1 : bs_code == 7 ? 2 : 0;
    len += sr_code == 12 ? 1 : sr_code == 13 || sr_code == 14 ? 2 : 0;
    return len < n ? len : 0;
}

/* After mutation: make the header checksum right again, and often the
 * frame checksum, so that the damage reaches the decoder proper. */
static void fixup(Buf *b, const Cfg *cfg)
{
    int len;

    if (rnd(6) == 0 || !(len = header_length(b->d, b->n)))
        return;
    b->d[len] = crc8(b->d, len);
    if (rnd(3) && b->n > len + 3) {
        uint16_t c = crc16(b->d, b->n - 2);
        b->d[b->n - 2] = c >> 8;
        b->d[b->n - 1] = c;
    }
}

/* ---- benchmark streams ---- */

typedef struct Path {
    const char *name;
    int bps;
    int channels;               /* 0: as the benchmark asks (two) */
    int assignment;             /* 0 independent, 8 left/side, 9 side/right, 10 mid/side */
    int type;                   /* 0 constant, 1 verbatim, 2 fixed, 3 LPC */
    int order;
    int prec;                   /* LPC coefficient bits */
    int wasted;
    int escape;                 /* residual as raw bits */
    int opt0, opt1;             /* request_sample_fmt index, err_recognition */
} Path;

static const Path paths[] = {
    { "const-16",      16, 0,  0, 0 },
    { "verbatim-16",   16, 0,  0, 1 },
    { "fixed-16-ms",   16, 0, 10, 2,  2 },
    { "fixed-16-ls",   16, 0,  8, 2,  3 },
    { "lpc-16-o8",     16, 0,  0, 3,  8, 12 },
    { "lpc-16-o12-ms", 16, 0, 10, 3, 12, 12 },
    { "lpc-16-o32",    16, 0,  0, 3, 32, 10 },
    { "lpc-16-wide",   16, 0, 10, 3,  8, 15 },   /* 15-bit coefficients: the 64-bit sum */
    { "lpc-24-o8-ms",  24, 0, 10, 3,  8, 15 },
    { "lpc-24-o12",    24, 0,  0, 3, 12, 15 },
    { "lpc-24-o32",    24, 0,  0, 3, 32, 15 },
    { "lpc-32-side",   32, 0,  8, 3,  8, 15 },   /* a 33-bit side channel */
    { "fixed-24-wast", 24, 0,  0, 2,  2,  0, 8 },
    { "escape-16",     16, 0,  0, 2,  1,  0, 0, 1 },
    { "lpc-16-6ch",    16, 6,  0, 3,  8, 12 },
    { "lpc-24-8ch",    24, 8,  0, 3,  8, 15 },
    { "lpc-16-planar", 16, 0, 10, 3,  8, 12, 0, 0, 3 },
    { "lpc-16-crc",    16, 0, 10, 3,  8, 12, 0, 0, 0, AV_EF_CRCCHECK },
};

static const char synth_kinds[] =
    "const-16 verbatim-16 fixed-16-ms fixed-16-ls lpc-16-o8 lpc-16-o12-ms lpc-16-o32 "
    "lpc-16-wide lpc-24-o8-ms lpc-24-o12 lpc-24-o32 lpc-32-side fixed-24-wast escape-16 "
    "lpc-16-6ch lpc-24-8ch lpc-16-planar lpc-16-crc";

#define SYNTH_MAX 16384
static int64_t sig[8][SYNTH_MAX];

/* A made-up signal: two tones and some noise per channel, each channel
 * sharing most of the first one's signal; at most 0.8 of full scale. */
static void make_signal(int channels, int n, int bps)
{
    double full = ldexp(1.0, bps - 1), base[SYNTH_MAX];
    double f1 = 0.002 + rnd(1000) * 2e-5, f2 = 0.02 + rnd(1000) * 5e-5;
    double p1 = rnd(628) * 0.01, p2 = rnd(628) * 0.01, lp = 0;

    for (int i = 0; i < n; i++) {
        lp = 0.9 * lp + 0.1 * ((int)rnd(2001) - 1000) * 1e-3;
        base[i] = 0.35 * sin(6.2831853 * f1 * i + p1) + 0.12 * sin(6.2831853 * f2 * i + p2) +
                  0.02 * lp;
    }
    for (int ch = 0; ch < channels; ch++) {
        double g = ch ? 0.5 + rnd(40) * 0.01 : 1.0, f3 = 0.005 + rnd(1000) * 3e-5, lp2 = 0;
        for (int i = 0; i < n; i++) {
            lp2 = 0.8 * lp2 + 0.2 * ((int)rnd(2001) - 1000) * 1e-3;
            sig[ch][i] = (int64_t)(full * (g * base[i] + (ch ? 0.1 * sin(6.2831853 * f3 * i) : 0) +
                                           0.004 * lp2 + ((int)rnd(2001) - 1000) * 2e-6));
        }
    }
}

static uint32_t fold(int64_t r)
{
    return (uint32_t)((r << 1) ^ (r >> 63));
}

static void synth_residual(BW *w, const int64_t *res, int n, int order, int escape)
{
    int po = 4, wide = 0, ks[16];

    while (po > 0 && ((n & ((1 << po) - 1)) || (n >> po) < order))
        po--;
    for (int part = 0; part < 1 << po; part++) {
        int from = part * (n >> po), to = from + (n >> po);
        uint64_t sum = 0;
        int k = 0;
        if (!part)
            from = order;
        for (int i = from; i < to; i++)
            sum += fold(res[i]);
        if (to > from)
            sum /= to - from;
        while (k < 30 && (sum >> (k + 1)))
            k++;
        if (escape) {                           /* the bits the largest value needs */
            int64_t m = 0;
            for (int i = from; i < to; i++)
                m |= res[i] < 0 ? ~res[i] : res[i];
            for (k = 1; k < 31 && (m >> (k - 1)); k++)
                ;
        }
        ks[part] = k;
        if (k > 14)
            wide = 1;
    }
    bw_put(w, 2, wide);
    bw_put(w, 4, po);
    for (int part = 0; part < 1 << po; part++) {
        int from = part ? part * (n >> po) : order, to = (part + 1) * (n >> po);
        if (escape) {
            bw_put(w, 4 + wide, wide ? 31 : 15);
            bw_put(w, 5, ks[part]);
            for (int i = from; i < to; i++)
                bw_put(w, ks[part], res[i]);
        } else {
            bw_put(w, 4 + wide, ks[part]);
            for (int i = from; i < to; i++)
                bw_rice(w, ks[part], fold(res[i]));
        }
    }
}

static void synth_subframe(BW *w, const Path *p, int64_t *x, int n, int bps)
{
    static int64_t res[SYNTH_MAX];
    int order = p->order;

    bw_put(w, 1, 0);
    bw_put(w, 6, p->type == 0 ? 0 : p->type == 1 ? 1 : p->type == 2 ? 8 + order : 32 + order - 1);
    if (p->wasted) {
        bw_put(w, 1, 1);
        bw_zeros(w, p->wasted - 1);
        bw_put(w, 1, 1);
        bps -= p->wasted;
        for (int i = 0; i < n; i++)
            x[i] >>= p->wasted;
        x[0] |= 1;                              /* (or more bits might be unused) */
    } else {
        bw_put(w, 1, 0);
    }

    if (p->type == 0) {
        bw_put64(w, bps, x[0]);
    } else if (p->type == 1) {
        for (int i = 0; i < n; i++)
            bw_put64(w, bps, x[i]);
    } else if (p->type == 2) {
        for (int i = 0; i < order; i++)
            bw_put64(w, bps, x[i]);
        for (int i = order; i < n; i++)
            res[i] = order == 0 ? x[i] :
                     order == 1 ? x[i] - x[i - 1] :
                     order == 2 ? x[i] - 2 * x[i - 1] + x[i - 2] :
                     order == 3 ? x[i] - 3 * x[i - 1] + 3 * x[i - 2] - x[i - 3] :
                                  x[i] - 4 * x[i - 1] + 6 * x[i - 2] - 4 * x[i - 3] + x[i - 4];
        synth_residual(w, res, n, order, p->escape);
    } else {
        /* A predictor that does well on smooth signals (1.6 and -0.7 on
         * the last two samples) with small taps further back. coef[0]
         * applies to the previous sample. */
        int coef[32], qlevel = p->prec - 2;
        for (int j = 0; j < order; j++) {
            double c = j == 0 ? 1.6 : j == 1 ? -0.7 : ((int)rnd(81) - 40) * 1e-3;
            coef[j] = (int)lrint(ldexp(c, qlevel));
        }
        for (int i = 0; i < order; i++)
            bw_put64(w, bps, x[i]);
        bw_put(w, 4, p->prec - 1);
        bw_put(w, 5, qlevel);
        for (int j = 0; j < order; j++)
            bw_put(w, p->prec, coef[j]);
        for (int i = order; i < n; i++) {
            int64_t sum = 0;
            for (int j = 0; j < order; j++)
                sum += coef[j] * x[i - 1 - j];
            res[i] = x[i] - (sum >> qlevel);
        }
        synth_residual(w, res, n, order, p->escape);
    }
}

static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    const Path *p = NULL;
    BW w = { &b };
    AVPacket *pkt;
    uint8_t number = 0;
    int n = cfg->w, channels;

    for (size_t i = 0; i < FF_ARRAY_ELEMS(paths); i++)
        if (!strcmp(kind, paths[i].name))
            p = &paths[i];
    if (!p || n < 64 || n > SYNTH_MAX)
        return NULL;
    channels = p->channels ? p->channels : 2;
    cfg->sample_rate = p->bps > 16 ? 96000 : 44100;
    cfg->channels    = channels;
    cfg->opt[0]      = p->opt0;
    cfg->opt[1]      = p->opt1;

    make_signal(channels, n, p->bps);
    if (p->assignment) {                        /* stereo: left and right are in 0 and 1 */
        for (int i = 0; i < n; i++) {
            int64_t l = sig[0][i], r = sig[1][i];
            if (p->assignment == 9)
                sig[0][i] = l - r;
            else if (p->assignment == 10)
                sig[0][i] = (l + r) >> 1;
            if (p->assignment != 9)
                sig[1][i] = l - r;
        }
    }

    b.n = 0;
    frame_header(&w, n, -1, p->bps > 16 ? 11 : 9, 0,
                 p->assignment ? p->assignment : channels - 1, bps_code_of(p->bps), &number, 1);
    for (int ch = 0; ch < channels; ch++) {
        int side = (p->assignment == 8 && ch == 1) || (p->assignment == 9 && ch == 0) ||
                   (p->assignment == 10 && ch == 1);
        synth_subframe(&w, p, sig[ch], n, p->bps + side);
    }
    frame_footer(&w, 0);

    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = {
    0x00, 0xff, 0xf8, 0xf9, 0x04, 0x84, 0x7f, 0x80, 0x01, 0x02, 0xfe, 0x10, 0x3f, 'f', 'L', 'a', 'C',
};

const CodecOps ops_flac = {
    .name           = "flac",
    .c_name         = "flac",
    .rs_name        = "flac_rs",
    .id             = AV_CODEC_ID_FLAC,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .options        = options,
    .setup          = setup,
    .fixup          = fixup,
    .synth_kinds    = synth_kinds,
    .synth          = synth,
};
