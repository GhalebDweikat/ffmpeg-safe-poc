/* Apple ProRes: packet generators. */

#include <stdlib.h>
#include <string.h>

#include "libavutil/common.h"
#include "libavutil/cpu.h"
#include "libavutil/macros.h"

#include "common.h"

/* Test hook in the Rust library: 0 forces the portable decoder. */
void prores_rs_set_simd(int enabled);

/* ---- bit writer and the encoder side of the codewords ---- */

typedef struct BW {
    Buf *b;
    uint32_t acc;
    int n;
} BW;

static void bw_put(BW *w, int bits, uint32_t v)
{
    for (int i = bits - 1; i >= 0; i--) {
        w->acc = w->acc << 1 | ((v >> i) & 1);
        if (++w->n == 8) {
            put(w->b, w->acc);
            w->acc = 0;
            w->n   = 0;
        }
    }
}

/* Pad to a whole byte: with zeros as encoders do, or with ones. */
static void bw_flush(BW *w, int ones)
{
    while (w->n)
        bw_put(w, 1, ones);
}

static int ilog2(uint32_t v)
{
    int n = 0;
    while (v >>= 1)
        n++;
    return n;
}

/* encode_vlc_codeword() of FFmpeg's ProRes encoders. */
static void put_cw(BW *w, unsigned codebook, uint32_t val)
{
    unsigned switch_bits = (codebook & 3) + 1;
    unsigned rice_order  = codebook >> 5;
    unsigned exp_order   = (codebook >> 2) & 7;
    unsigned switch_val  = switch_bits << rice_order;

    if (val >= switch_val) {
        int exponent;
        val -= switch_val - (1 << exp_order);
        exponent = ilog2(val);
        bw_put(w, exponent - exp_order + switch_bits, 0);
        bw_put(w, exponent + 1, val);
    } else {
        bw_put(w, val >> rice_order, 0);
        bw_put(w, 1, 1);
        if (rice_order)
            bw_put(w, rice_order, val);
    }
}

static const uint8_t dc_cb[7]   = { 0x04, 0x28, 0x28, 0x4D, 0x4D, 0x70, 0x70 };
static const uint8_t run_cb[16] = { 0x06, 0x06, 0x05, 0x05, 0x04, 0x29, 0x29, 0x29,
                                    0x29, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x4C };
static const uint8_t lev_cb[10] = { 0x04, 0x0A, 0x05, 0x06, 0x04, 0x28, 0x28, 0x28, 0x28, 0x4C };

/* ---- one frame ---- */

/* How the coefficients of a plane are chosen. */
typedef struct Style {
    int dc_first;               /* range of the first DC code */
    int dc_delta;               /* range of the DC difference codes */
    int mean_run;               /* mean zero run between coefficients; < 0: no AC */
    int level;                  /* range of the level codes */
    int overrun;                /* run past the last coefficient (an error) */
    int ones;                   /* pad with one bits */
    int edge;                   /* levels on the wrap-around points of the IDCTs */
} Style;

typedef struct Params {
    int w, h;
    int chroma444;
    int frame_type;             /* 0 progressive, 1 tff, 2 and 3 bff */
    int alpha;                  /* 0 none, 1: 8 bits, 2: 16 bits */
    int log2_slice_mb_width;
    int qmat_flags;             /* bit 1: luma matrix, bit 0: chroma matrix */
    int pictures;
    int qscale;                 /* < 0: random per slice */
    int fuzz;                   /* sprinkle broken things */
    int qmat_ones;              /* quantisation matrices of ones */
    Style luma, chroma;
} Params;

static void be16(Buf *b, int v)
{
    put(b, v >> 8); put(b, v);
}

static void patch16(Buf *b, int at, int v)
{
    b->d[at] = v >> 8; b->d[at + 1] = v;
}

static void patch32(Buf *b, int at, uint32_t v)
{
    b->d[at] = v >> 24; b->d[at + 1] = v >> 16; b->d[at + 2] = v >> 8; b->d[at + 3] = v;
}

/* Coefficient magnitudes at which FFmpeg's inverse DCTs wrap around or
 * saturate, and at which its assembly and C versions part ways. */
static const unsigned edges[] = { 32767, 32768, 32766, 24575, 24568, 16384, 8192, 1 };

static void gen_plane(Buf *b, int blocks, const Style *st)
{
    BW w = { b };
    unsigned max_coeffs = 64 * blocks, pos = blocks - 1;
    unsigned code, run = 4, level = 2;

    put_cw(&w, 0xB8, st->edge ? 65534 + rnd(2) : rnd(st->dc_first));
    code = 5;
    for (int i = 1; i < blocks; i++) {
        unsigned v = rnd(st->dc_delta);
        put_cw(&w, dc_cb[FFMIN(code, 6)], v);
        code = v;
    }
    while (st->mean_run >= 0) {
        unsigned r = rnd(2 * st->mean_run + 1), lv;
        if (pos + r + 1 >= max_coeffs && !st->overrun)
            break;
        pos += r + 1;
        lv = rnd(st->level);
        if (rnd(4))                     /* small levels are the common case */
            lv = rnd(FFMIN(st->level, 3));
        if (st->edge && rnd(3))
            lv = edges[rnd(8)] - 1;
        put_cw(&w, run_cb[FFMIN(run, 15)], r);
        put_cw(&w, lev_cb[FFMIN(level, 9)], lv);
        bw_put(&w, 1, rnd(2));
        run   = r;
        level = lv + 1;
        if (pos >= max_coeffs)
            break;
    }
    bw_flush(&w, st->ones);
}

static void gen_alpha(Buf *b, int samples, int bits)
{
    BW w = { b };
    int idx = 0;

    while (idx < samples) {
        int n = 1 + (rnd(4) ? 0 : rnd(6));
        for (int i = 0; i < n; i++) {
            if (i)
                bw_put(&w, 1, 1);       /* another value follows */
            if (rnd(3) == 0) {
                bw_put(&w, 1, 1);
                bw_put(&w, bits, rnd(1 << bits));
            } else {
                bw_put(&w, 1, 0);
                bw_put(&w, bits == 16 ? 7 : 4, rnd(128));
            }
            idx++;
        }
        bw_put(&w, 1, 0);
        n = rnd(4) ? 1 + rnd(15) : rnd(3) ? 16 + rnd(200) : rnd(2048);
        if (n && n < 16) {
            bw_put(&w, 4, n);
        } else {
            bw_put(&w, 4, 0);
            bw_put(&w, 11, n);
        }
        idx += n;
    }
    bw_flush(&w, 0);
}

static void random_bytes(Buf *b, int n)
{
    while (n-- > 0)
        put(b, rnd(256));
}

static void gen_slice(Buf *b, int mb_count, const Params *p)
{
    int start = b->n, at, sizes[3];
    int hdr = p->alpha || rnd(4) == 0 ? 8 : 6;
    int qscale = p->qscale >= 0 ? p->qscale : rnd(3) ? 1 + rnd(16) : rnd(256);

    if (p->fuzz && rnd(200) == 0)
        hdr = rnd(32);
    put(b, hdr << 3 | (p->fuzz ? rnd(8) : 0));
    put(b, qscale);
    for (int i = 0; i < (hdr > 7 ? 3 : 2); i++)
        be16(b, 0);
    while (b->n - start < hdr)
        put(b, rnd(256));

    for (int i = 0; i < 3; i++) {
        const Style *st = i ? &p->chroma : &p->luma;
        int blocks = i ? mb_count << (p->chroma444 ? 2 : 1) : mb_count << 2;
        at = b->n;
        if (p->fuzz && rnd(64) == 0)
            random_bytes(b, rnd(40));
        else if (!(i && p->fuzz && rnd(16) == 0))       /* else an empty plane */
            gen_plane(b, blocks, st);
        sizes[i] = b->n - at;
        if (p->fuzz && rnd(100) == 0)
            sizes[i] += (int)rnd(9) - 4;
    }
    if (p->alpha) {
        if (p->fuzz && rnd(32) == 0)
            random_bytes(b, rnd(60));
        else if (!(p->fuzz && rnd(32) == 0))
            gen_alpha(b, mb_count * 256, p->alpha == 2 ? 16 : 8);
    }
    for (int i = 0; i < (hdr > 7 ? 3 : 2); i++)
        if (start + 2 + 2 * i + 2 <= b->n)
            patch16(b, start + 2 + 2 * i, sizes[i] & 0xffff);
}

static void gen_picture(Buf *b, const Params *p)
{
    int start = b->n, index, slices;
    int mb_w = (p->w + 15) >> 4;
    int mb_h = p->frame_type ? (p->h + 31) >> 5 : (p->h + 15) >> 4;
    int slice_w = 1 << p->log2_slice_mb_width;

    slices = mb_h * ((mb_w >> p->log2_slice_mb_width) + av_popcount(mb_w & (slice_w - 1)));
    put(b, 8 << 3);
    for (int i = 0; i < 4; i++)
        put(b, 0);                      /* picture size, patched below */
    be16(b, slices);
    put(b, p->log2_slice_mb_width << 4);
    index = b->n;
    for (int i = 0; i < slices; i++)
        be16(b, 0);
    for (int y = 0, i = 0; y < mb_h; y++) {
        int count = slice_w;
        for (int x = 0; x < mb_w; x += count, i++) {
            int at = b->n;
            while (mb_w - x < count)
                count >>= 1;
            gen_slice(b, count, p);
            patch16(b, index + 2 * i, b->n - at);
        }
    }
    patch32(b, start + 1, b->n - start);
}

static void gen_frame(Buf *b, const Params *p)
{
    b->n = 0;
    for (int i = 0; i < 4; i++)
        put(b, 0);                      /* frame size, patched below */
    put(b, 'i'); put(b, 'c'); put(b, 'p'); put(b, 'f');
    be16(b, 20 + 64 * (!!(p->qmat_flags & 2) + !!(p->qmat_flags & 1)));
    be16(b, 0);                         /* version */
    put(b, 'f'); put(b, 'u'); put(b, 'z'); put(b, 'z');
    be16(b, p->w);
    be16(b, p->h);
    put(b, (p->chroma444 ? 0xC0 : 0x80) | p->frame_type << 2);
    put(b, 0);
    put(b, p->fuzz ? rnd(12) : 1);      /* primaries, transfer, matrix */
    put(b, p->fuzz ? rnd(20) : 1);
    put(b, p->fuzz ? rnd(12) : 1);
    put(b, p->alpha);
    put(b, 0);
    put(b, p->qmat_flags);
    for (int m = 0; m < 2; m++)
        if (p->qmat_flags & (2 >> m))
            for (int i = 0; i < 64; i++)
                put(b, p->qmat_ones ? 1 : rnd(4) ? 1 + rnd(8) : rnd(256));
    for (int i = 0; i < p->pictures; i++)
        gen_picture(b, p);
    patch32(b, 0, b->n);
}

/* ---- fuzzing ---- */

/* Chosen per fuzz case, so that most packets of a case share a layout and
 * a slice that fails shows what an earlier frame left in the buffer. */
static Params fuzz_params;

static void random_style(Style *st)
{
    static const int runs[] = { -1, 0, 1, 3, 10, 40 };
    static const int levels[] = { 2, 4, 40, 1000, 70000 };

    st->dc_first = rnd(4) ? 1 + rnd(4096) : 1 + rnd(1 << 17);
    st->dc_delta = rnd(4) ? 1 + rnd(16) : 1 + rnd(1 << 16);
    st->mean_run = runs[rnd(6)];
    st->level    = levels[rnd(5)];
    st->overrun  = rnd(40) == 0;
    st->ones     = rnd(10) == 0;
    st->edge     = rnd(6) == 0;
}

static void random_layout(Params *p)
{
    p->chroma444  = rnd(2);
    p->frame_type = rnd(2) ? 0 : 1 + rnd(3);
    p->alpha      = rnd(2) ? 0 : 1 + rnd(2);
    p->log2_slice_mb_width = rnd(4);
    p->qmat_flags = rnd(4);
    p->qscale     = -1;
    p->fuzz       = 1;
}

static void config(Cfg *cfg)
{
    static const char tags[][5] = { "apch", "apcn", "apcs", "apco", "ap4h", "ap4x", "xxxx" };
    const char *t = tags[rnd(7)];

    cfg->tag = MKTAG(t[0], t[1], t[2], t[3]);
    random_layout(&fuzz_params);
}

static void gen(Buf *b, const Cfg *cfg)
{
    Params p = fuzz_params;

    if (rnd(8) == 0) {
        random_layout(&p);
        if (rnd(2))
            fuzz_params = p;
    }
    p.w = cfg->w;
    p.h = cfg->h;
    if (rnd(16) == 0) {                 /* a change of picture size */
        p.w = 1 + rnd(70);
        p.h = 1 + rnd(70);
    }
    p.pictures = p.frame_type ? 2 : 1;
    if (rnd(24) == 0)
        p.pictures = 1 + rnd(3);
    if (rnd(64) == 0)
        p.alpha = rnd(16);
    if (rnd(64) == 0)
        p.log2_slice_mb_width = rnd(16);
    if (rnd(6) == 0) {                  /* coefficients reach the IDCT unscaled */
        p.qmat_ones  = 1;
        p.qmat_flags = 3;
        p.qscale     = 1;
    }
    random_style(&p.luma);
    random_style(&p.chroma);
    gen_frame(b, &p);
    if (rnd(64) == 0)
        b->d[11] = rnd(3);              /* version */
    if (rnd(64) == 0)
        b->d[8 + 12] = rnd(256);        /* chroma format, frame type */
}

/* The codec-specific choices of a case: bit 0 of opt[0] skips alpha, bit 1
 * leaves the chroma out (the "gray" flag); opt[1] makes FFmpeg (and with
 * it the port) use the C inverse DCT instead of the assembly; opt[2] marks
 * a fuzz case. */
static void options(Cfg *cfg)
{
    cfg->opt[0] = (rnd(6) == 0) | (rnd(8) == 0) << 1;
    cfg->opt[1] = rnd(4) == 0;
    cfg->opt[2] = 1;
    /* Decided by the decoders when they are opened, which is next. */
    av_force_cpu_flags(cfg->opt[1] ? 0 : -1);
    /* A quarter of the cases run the Rust decoder without AVX2. */
    prores_rs_set_simd(rnd(4) != 0);
}

static void setup(AVCodecContext *ctx, const Cfg *cfg)
{
    ctx->skip_alpha = cfg->opt[0] & 1;
    if (cfg->opt[0] & 2)
        ctx->flags |= AV_CODEC_FLAG_GRAY;
    /* A mutated header can ask for a picture of up to 65535 x 65535; both
     * decoders would allocate and clear it before finding that the packet
     * has no data for it. Refuse those sizes, for both alike. */
    if (cfg->opt[2])
        ctx->max_pixels = 1 << 21;
}

/* ---- benchmark: single-path streams ---- */

static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    static const Style dc     = { 2048, 8, -1, 0 };
    static const Style sparse = { 2048, 8, 24, 3 };
    static const Style medium = { 2048, 8, 6, 4 };
    static const Style dense  = { 2048, 16, 1, 12 };
    Params p = { .w = cfg->w, .h = cfg->h, .log2_slice_mb_width = 3, .pictures = 1, .qscale = 4 };
    const Style *st;
    AVPacket *pkt;
    const char *rest;

    if      (!strncmp(kind, "dc-", 3))     { st = &dc;     rest = kind + 3; }
    else if (!strncmp(kind, "sparse-", 7)) { st = &sparse; rest = kind + 7; }
    else if (!strncmp(kind, "medium-", 7)) { st = &medium; rest = kind + 7; }
    else if (!strncmp(kind, "dense-", 6))  { st = &dense;  rest = kind + 6; }
    else return NULL;
    p.luma = p.chroma = *st;

    if (!strcmp(rest, "422")) {                 /* 10 bits, 4:2:2 */
        cfg->tag = MKTAG('a', 'p', 'c', 'h');
    } else if (!strcmp(rest, "422i")) {         /* the same in two fields */
        cfg->tag = MKTAG('a', 'p', 'c', 'h');
        p.frame_type = 1;
        p.pictures   = 2;
    } else if (!strcmp(rest, "444-12")) {       /* 12 bits, 4:4:4 */
        cfg->tag = MKTAG('a', 'p', '4', 'h');
        p.chroma444 = 1;
    } else if (!strcmp(rest, "alpha16-12")) {   /* 12 bits, 4:4:4, 16-bit alpha */
        cfg->tag = MKTAG('a', 'p', '4', 'h');
        p.chroma444 = 1;
        p.alpha     = 2;
    } else if (!strcmp(rest, "alpha8-10")) {    /* 10 bits, 4:4:4, 8-bit alpha */
        cfg->tag = MKTAG('a', 'p', 'c', 'h');
        p.chroma444 = 1;
        p.alpha     = 1;
    } else {
        return NULL;
    }
    gen_frame(&b, &p);
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = { 0x00, 0xff, 0x80, 0xC0, 0x40, 0x30, 0x01, 0x02, 0x7f, 0xe0, 0x08 };

const CodecOps ops_prores = {
    .name           = "prores",
    .c_name         = "prores",
    .rs_name        = "prores_rs",
    .id             = AV_CODEC_ID_PRORES,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .options        = options,
    .setup          = setup,
    .synth_kinds    = "dc-422 sparse-422 medium-422 dense-422 medium-422i dc-444-12 sparse-444-12 "
                      "medium-444-12 dense-444-12 dc-alpha16-12 dc-alpha8-10",
    .synth          = synth,
};
