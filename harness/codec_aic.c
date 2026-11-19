/* Apple Intermediate Codec: packet generators.
 *
 * FFmpeg has no AIC encoder, so this file has a small one: it writes a
 * valid frame header and slice table, and slices whose four coefficient
 * bands hold random values in any of the coding modes (unary or Exp-Golomb
 * codes with 0 to 7 extra bits, with or without zero runs). It does not
 * transform a picture; the coefficients are random numbers. */

#include <stdlib.h>
#include <string.h>

#include "libavutil/cpu.h"

#include "common.h"

#define HDR_SIZE 24

/* Test hook in the Rust library: 0 makes new decoders use the portable
 * code instead of SSE2. */
void aic_rs_set_simd(int enabled);

static const int band_coeffs[4] = { 64, 32, 192, 96 };

/* What aic_decode_init() derives from the picture size. */
typedef struct Geo {
    int mb_w, mb_h, slice_w, num_x;
} Geo;

static Geo geometry(int w, int h)
{
    Geo g;

    g.mb_w    = (w + 15) >> 4;
    g.mb_h    = (h + 15) >> 4;
    g.num_x   = (g.mb_w + 15) >> 4;
    g.slice_w = 16;
    for (int i = 1; i < g.mb_w; i++)
        if (!(g.mb_w % i) && g.mb_w / i <= 32) {
            g.slice_w = g.mb_w / i;
            g.num_x   = i;
            break;
        }
    return g;
}

/* Bits, most significant first. */
static uint64_t acc;
static int nacc;

static void bits(Buf *b, int n, uint32_t v)
{
    if (!n)
        return;
    acc   = acc << n | (v & (n == 32 ? ~0u : (1u << n) - 1));
    nacc += n;
    while (nacc >= 8) {
        put(b, acc >> (nacc - 8));
        nacc -= 8;
    }
}

static void flush_bits(Buf *b)
{
    if (nacc)
        bits(b, 8 - nacc, 0);
}

/* The largest value a code of this kind can carry. */
static int max_code(int golomb, int add_bits)
{
    int hi = golomb ? 8190 : 31;
    return (hi << add_bits) | ((1 << add_bits) - 1);
}

/* GET_CODE's counterpart. Returns the value written, which is v unless v
 * is too large for this kind of code. */
static int code(Buf *b, int golomb, int add_bits, int v)
{
    int hi, lim = max_code(golomb, add_bits);

    if (v > lim)
        v = lim;
    hi = v >> add_bits;
    if (golomb) {
        int len = 32 - __builtin_clz(hi + 1);
        bits(b, len - 1, 0);
        bits(b, len, hi + 1);
    } else {
        bits(b, hi, 0);
        if (hi < 31)
            bits(b, 1, 1);
    }
    bits(b, add_bits, v);
    return v;
}

/* How the coefficients of a packet are chosen. */
typedef struct Style {
    int skips;          /* 0, 1, or -1 for either */
    int golomb;         /* 0, 1, or -1 for either */
    int add_bits;       /* 0 to 7, or -1 for any */
    int skip_bits;      /* extra bits of the zero-run codes, unless add_bits is -1 */
    int per_mb;         /* in skip mode: coefficients coded per 64, at most */
    int range;          /* 0: small values; 1: any 16-bit value; 2: mostly extremes */
    int dc_only;        /* in skip mode: only the first coefficient of each block */
} Style;

/* A coded coefficient: bit 0 is the sign, the rest the magnitude. */
static int coefficient(const Style *st)
{
    switch (st->range) {
    case 0:
        return rnd(4) ? rnd(16) : rnd(8) ? rnd(128) : rnd(1024);
    case 1:
        return rnd(3) ? rnd(64) : rnd(65536);
    default:
        switch (rnd(6)) {
        case 0:  return 0xffff;
        case 1:  return 0xfffe;
        case 2:  return 0xffff - rnd(64);
        case 3:  return rnd(65536);
        default: return rnd(256);
        }
    }
}

static void gen_band(Buf *b, int band, int mbs, const Style *st)
{
    int n          = band_coeffs[band];
    int has_skips  = st->skips    < 0 ? rnd(2) : st->skips;
    int coeff_type = st->golomb   < 0 ? rnd(2) : st->golomb;
    int coeff_bits = st->add_bits < 0 ? rnd(8) : st->add_bits;

    bits(b, 1, has_skips);
    bits(b, 1, coeff_type);
    bits(b, 3, coeff_bits);

    if (has_skips) {
        int skip_type = st->golomb < 0 ? rnd(2) : st->golomb;
        int skip_bits = st->add_bits < 0 ? rnd(8) : st->skip_bits;
        int lim       = max_code(skip_type, skip_bits);

        if (lim > 0xffff)
            lim = 0xffff;
        bits(b, 1, skip_type);
        bits(b, 3, skip_bits);
        for (int mb = 0; mb < mbs; mb++) {
            int idx  = -1;
            int todo = st->per_mb ? rnd(st->per_mb * n / 64 + 1) : rnd(n + 1);
            for (;;) {
                int end = n - idx - 1;      /* the smallest skip that ends the macroblock */
                int skip, c;
                if (st->dc_only) {
                    /* The first coefficient of each block, in coding
                     * order (see aic_c_scan), and nothing in the
                     * extension bands. */
                    int next = band >= 2 ? n : idx < 0 ? 0 : idx < 31 ? 31 : idx < 32 ? 32 : idx < 63 ? 63 : n;
                    skip = next - idx - 1;
                    todo = next < n;
                } else {
                    skip = rnd(4) ? rnd(4) : rnd(2 * n / (todo > 0 ? todo + 1 : 1) + 1);
                }
                if (todo <= 0 || skip >= end) {
                    if (end <= lim) {
                        code(b, skip_type, skip_bits, rnd(8) ? end : end + rnd(lim - end + 1));
                        break;
                    }
                    skip = lim;             /* too far for one code: go on */
                }
                idx += code(b, skip_type, skip_bits, skip) + 1;
                c = coefficient(st);        /* the decoder adds 1 */
                code(b, coeff_type, coeff_bits, c > 0xfffe ? 0xfffe : c);
                todo--;
                if (idx >= n - 1)
                    break;
            }
        }
    } else {
        for (int i = 0; i < mbs * n; i++)
            code(b, coeff_type, coeff_bits, coefficient(st));
    }
}

static void be16(uint8_t *p, int v) { p[0] = v >> 8; p[1] = v; }
static void be32(uint8_t *p, uint32_t v) { be16(p, v >> 16); be16(p + 2, v); }

/* A frame. With damage set, some slices are declared shorter or longer
 * than they are, which the decoder accepts as long as it finds codes. */
static void gen_frame(Buf *b, int w, int h, int quant, int interlaced,
                      const Style *st, int damage)
{
    Geo g      = geometry(w, h);
    int table  = HDR_SIZE, nslices = g.num_x * g.mb_h;
    int k      = 0;

    b->n = 0;
    for (int i = 0; i < HDR_SIZE; i++)
        put(b, rnd(256));
    b->d[0] = 1;
    b->d[1] = HDR_SIZE - 2;
    be16(b->d + 6, w);
    be16(b->d + 8, h);
    b->d[15] = quant;
    b->d[16] = interlaced ? 0x30 | rnd(16) : rnd(2) ? 0 : (b->d[16] >> 4 == 3 ? 0 : b->d[16]);
    for (int i = 0; i < 2 * nslices; i++)
        put(b, 0);
    while (b->n & 3)
        put(b, 0);

    for (int y = 0; y < g.mb_h; y++)
        for (int x = 0; x < g.mb_w; x += g.slice_w, k++) {
            int mbs = g.mb_w - x < g.slice_w ? g.mb_w - x : g.slice_w;
            int start = b->n, words;

            if (b->n > MAX_PKT - (1 << 18))
                break;
            acc = nacc = 0;
            for (int band = 0; band < 4; band++)
                gen_band(b, band, mbs, st);
            flush_bits(b);
            while ((b->n - start) & 3)
                put(b, damage ? rnd(256) : 0);
            words = (b->n - start) / 4;
            if (damage && rnd(6) == 0) {
                /* Shorter (the next slice then starts inside this one's
                 * data), or longer with random bytes. */
                if (rnd(2) && words > 1) {
                    words -= 1 + rnd(words > 4 ? 3 : 1);
                    if (rnd(2))
                        b->n = start + 4 * words;
                } else {
                    for (int i = 4 * (1 + rnd(3)); i > 0; i--)
                        put(b, rnd(256));
                    words = (b->n - start) / 4;
                }
            }
            if (words > 0xffff)
                words = 0xffff;
            b->d[table + 2 * k]     = words;
            b->d[table + 2 * k + 1] = words >> 8;
        }
    be32(b->d + 2, b->n);
}

static void config(Cfg *cfg)
{
    /* Up to 512 pixels wide a macroblock row is one slice. Wider pictures
     * have several; where the width in macroblocks has no suitable
     * divisor the last one is narrower. */
    switch (rnd(32)) {
    case 0: cfg->w = 577 + rnd(16); cfg->h = 1 + rnd(36); break;    /* 37: 16 + 16 + 5 */
    case 1: cfg->w = 529 + rnd(16); cfg->h = 1 + rnd(36); break;    /* 34: 17 + 17 */
    case 2: cfg->w = 641 + rnd(16); cfg->h = 1 + rnd(20); break;    /* 41: 16 + 16 + 9 */
    case 3: cfg->w = 1024 - rnd(16); cfg->h = 1 + rnd(20); break;   /* 64: 32 + 32 */
    }
}

/* opt[0] chooses which IDCT and pixel store FFmpeg gives the C decoder;
 * the port has to follow. */
static void options(Cfg *cfg)
{
    switch (rnd(8)) {
    case 0:  cfg->opt[0] = 1; break;    /* no CPU extensions: all C */
    case 1:  cfg->opt[0] = 2; break;    /* C IDCT, SSE2 pixel store */
    case 2:  cfg->opt[0] = 3; break;    /* -idct simple, as in FFmpeg's tests */
    default: cfg->opt[0] = 0; break;
    }
    /* A quarter of the cases run the Rust decoder without SSE2. */
    cfg->opt[1] = rnd(4) == 0;
}

static void setup(AVCodecContext *ctx, const Cfg *cfg)
{
    av_force_cpu_flags(cfg->opt[0] == 1 ? 0 : -1);
    aic_rs_set_simd(!cfg->opt[1]);
    if (cfg->opt[0] == 2)
        ctx->idct_algo = FF_IDCT_SIMPLEARM;
    if (cfg->opt[0] == 3)
        ctx->idct_algo = FF_IDCT_SIMPLE;
}

static void gen(Buf *b, const Cfg *cfg)
{
    Style st = { .skips = -1, .golomb = -1, .add_bits = -1 };
    int quant;

    if (rnd(40) == 0) {
        int n = 1 + rnd(rnd(4) ? 64 : 2048);
        b->n = 0;
        for (int i = 0; i < n; i++)
            put(b, rnd(256));
        return;
    }
    st.range  = rnd(4) ? 0 : 1 + rnd(2);
    st.per_mb = rnd(3) ? 1 + rnd(12) : 0;
    if (rnd(3) == 0) {
        st.skips     = rnd(2);
        st.golomb    = rnd(2);
        st.add_bits  = rnd(8);
        st.skip_bits = rnd(8);
    }
    quant = rnd(4) ? 1 + rnd(32) : rnd(256);
    gen_frame(b, cfg->w, cfg->h, quant, rnd(3) == 0, &st, rnd(4) == 0);

    switch (rnd(40)) {
    case 0: be32(b->d + 2, b->n + 1 + rnd(8));      break;
    case 1: be32(b->d + 2, rnd(b->n + 1));          break;
    case 2: be16(b->d + 6 + 2 * rnd(2), rnd(300));  break;
    case 3: b->d[rnd(2)] = rnd(32);                 break;
    }
}

/* One frame in a single coding mode. The values are small, as in real
 * streams, so that the IDCT does not overflow. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    AVPacket *pkt;
    Style st = { .skips = 1, .golomb = 1, .add_bits = 2, .skip_bits = 1, .per_mb = 6 };
    int interlaced = 0;

    if (!strcmp(kind, "sparse")) {
        /* Zero runs, about three coefficients per block. */
    } else if (!strcmp(kind, "sparse-il")) {
        interlaced = 1;
    } else if (!strcmp(kind, "sparse-unary")) {
        st.golomb    = 0;
        st.skip_bits = 3;
    } else if (!strcmp(kind, "dense")) {
        st.skips = 0;                   /* every coefficient coded */
    } else if (!strcmp(kind, "dense-unary")) {
        st.skips  = 0;
        st.golomb = 0;
    } else if (!strcmp(kind, "dc")) {
        st.dc_only = 1;                 /* one coefficient per block */
    } else {
        return NULL;
    }
    gen_frame(&b, cfg->w, cfg->h, 8, interlaced, &st, 0);
    if (b.n >= MAX_PKT - (1 << 18))
        return NULL;
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = {
    0x00, 0x01, 0x02, 0x04, 0x08, 0x16, 0x30, 0x3f, 0x40, 0x7f, 0x80, 0xc0, 0xf8, 0xff,
};

const CodecOps ops_aic = {
    .name           = "aic",
    .c_name         = "aic",
    .rs_name        = "aic_rs",
    .id             = AV_CODEC_ID_AIC,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .options        = options,
    .setup          = setup,
    .synth_kinds    = "sparse sparse-il sparse-unary dense dense-unary dc",
    .synth          = synth,
};
