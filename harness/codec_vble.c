/* VBLE: a small encoder (FFmpeg has none) and the packet generators. */

#include <stdlib.h>
#include <string.h>

#include "libavutil/macros.h"

#include "common.h"

/* Test hook in the Rust library: 0 forces the portable median predictor. */
void vble_rs_set_simd(int enabled);

/*
 * The bitstream: a 32-bit version (1), then for every sample a code length
 * 0..8 written as that many zero bits and a one bit, then for every sample
 * with a non-zero length that many bits. Bits are filled from the least
 * significant end of each byte.
 *
 * A code is a number v: its length is floor(log2(v + 1)), its bits are
 * v + 1 - 2^length. The decoder turns v into the residual (v >> 1) ^ -(v & 1)
 * and keeps the low 8 bits, so v = 0..255 covers every residual and
 * v = 256..510 (also valid, never written by an encoder) covers them again.
 */

typedef struct BitW {
    Buf *b;
    uint64_t acc;
    int n;
} BitW;

static void put_bits(BitW *w, uint32_t v, int n)
{
    w->acc |= (uint64_t)v << w->n;
    w->n   += n;
    while (w->n >= 8) {
        put(w->b, (int)(w->acc & 0xff));
        w->acc >>= 8;
        w->n    -= 8;
    }
}

static int code_len(int v)
{
    int len = 0;
    while ((2 << len) <= v + 1)
        len++;
    return len;
}

/* Write a packet from n codes. */
static void write_packet(Buf *b, const uint16_t *codes, int n, uint32_t version)
{
    BitW w = { b, 0, 0 };

    b->n = 0;
    put(b, version); put(b, version >> 8); put(b, version >> 16); put(b, version >> 24);
    for (int i = 0; i < n; i++) {
        int len = code_len(codes[i]);
        put_bits(&w, 1u << len, len + 1);
    }
    for (int i = 0; i < n; i++) {
        int len = code_len(codes[i]);
        put_bits(&w, codes[i] + 1 - (1 << len), len);
    }
    if (w.n)
        put(b, (int)(w.acc & 0xff));
}

static int median(int a, int b, int c)
{
    if (a > b) { int t = a; a = b; b = t; }
    return c < a ? a : c > b ? b : c;
}

/* The codes of one plane: residuals against the decoder's prediction. */
static void plane_codes(uint16_t *codes, const uint8_t *p, int w, int h)
{
    for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++) {
            int pred, r;
            if (!y) {
                pred = x ? p[x - 1] : 0;
            } else {
                int t  = p[(y - 1) * w + x];
                int l  = x ? p[y * w + x - 1] : 0;
                int lt = x ? p[(y - 1) * w + x - 1] : t;
                pred = median(l, t, (l + t - lt) & 0xff);
            }
            r = (int8_t)(p[y * w + x] - pred);
            *codes++ = r >= 0 ? 2 * r : -2 * r - 1;
        }
}

enum { PIC_RANDOM, PIC_SMOOTH, PIC_FLAT, PIC_KINDS };

static void fill_plane(uint8_t *p, int w, int h, int kind)
{
    int base = rnd(256), dx = rnd(7), dy = rnd(7), noise = 1 + rnd(6);

    for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++)
            p[y * w + x] = kind == PIC_RANDOM ? rnd(256) :
                           kind == PIC_FLAT   ? base :
                           base + (x * dx + y * dy) / 4 + rnd(noise);
}

/* Scratch space for a picture and its codes, grown as needed. */
static uint8_t *pic;
static uint16_t *codes;
static int scratch_size;

static int symbol_count(int w, int h)
{
    return w * h + 2 * ((w + 1) / 2) * ((h + 1) / 2);
}

static void reserve(int n)
{
    if (n > scratch_size) {
        pic          = realloc(pic, n);
        codes        = realloc(codes, n * sizeof(*codes));
        scratch_size = n;
    }
}

/* Encode a picture of one kind per plane. The decoder reads a code for
 * every sample of a picture with rounded-up chroma planes but decodes
 * rounded-down ones, so for odd sizes there are codes left over; they get
 * the values in tail (or zero). */
static void encode_picture(Buf *b, int w, int h, const int kinds[3], int random_tail,
                           uint32_t version)
{
    int n = symbol_count(w, h), at = 0;

    reserve(n);
    for (int p = 0; p < 3; p++) {
        int pw = p ? w / 2 : w, ph = p ? h / 2 : h;
        fill_plane(pic, pw, ph, kinds[p]);
        plane_codes(codes + at, pic, pw, ph);
        at += pw * ph;
    }
    for (; at < n; at++)
        codes[at] = random_tail ? rnd(511) >> rnd(9) : 0;
    write_packet(b, codes, n, version);
}

static void config(Cfg *cfg)
{
    cfg->tag = MKTAG('V', 'B', 'L', 'E');
}

/* One case in eight decodes with AV_CODEC_FLAG_GRAY, which makes the
 * decoder skip the chroma planes. A quarter of the cases run the Rust
 * decoder without its SSE2 predictor. */
static void options(Cfg *cfg)
{
    cfg->opt[0] = rnd(8) == 0;
    vble_rs_set_simd(rnd(4) != 0);
}

static void setup(AVCodecContext *ctx, const Cfg *cfg)
{
    if (cfg->opt[0])
        ctx->flags |= AV_CODEC_FLAG_GRAY;
}

static void gen(Buf *b, const Cfg *cfg)
{
    int n = symbol_count(cfg->w, cfg->h);
    uint32_t version = rnd(8) ? 1 : rnd(4) ? rnd(4) : (uint32_t)rnd64();

    if (rnd(4)) {
        int kinds[3];
        for (int p = 0; p < 3; p++)
            kinds[p] = rnd(PIC_KINDS);
        encode_picture(b, cfg->w, cfg->h, kinds, rnd(2), version);
    } else {
        /* Codes that are not residuals of anything: every length, and the
         * values above 255 that an encoder never writes. */
        int shift = rnd(9), most = rnd(4) ? 511 : 256;
        reserve(n);
        for (int i = 0; i < n; i++)
            codes[i] = rnd(most) >> (rnd(3) ? shift : rnd(9));
        write_packet(b, codes, n, version);
    }

    switch (rnd(16)) {
    case 0:                     /* too short, by a little or a lot */
        b->n -= 1 + rnd(rnd(2) ? 2 : b->n);
        if (b->n < 0)
            b->n = 0;
        break;
    case 1:                     /* bytes after the end */
        for (int k = 1 + rnd(8); k > 0; k--)
            put(b, rnd(256));
        break;
    }
}

static const char *const synth_names[PIC_KINDS] = { "random", "smooth", "flat" };

/* One frame whose three planes are of one kind. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    AVPacket *pkt;
    int kinds[3], k;

    for (k = 0; k < PIC_KINDS; k++)
        if (!strcmp(kind, synth_names[k]))
            break;
    if (k == PIC_KINDS)
        return NULL;
    kinds[0] = kinds[1] = kinds[2] = k;
    config(cfg);
    encode_picture(&b, cfg->w, cfg->h, kinds, 0, 1);
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = { 0x00, 0xff, 0x01, 0x80, 0x55, 0xaa };

const CodecOps ops_vble = {
    .name           = "vble",
    .c_name         = "vble",
    .rs_name        = "vble_rs",
    .id             = AV_CODEC_ID_VBLE,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .options        = options,
    .setup          = setup,
    .synth_kinds    = "random smooth flat",
    .synth          = synth,
};
