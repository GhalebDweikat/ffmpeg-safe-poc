/* QuickTime Animation (RLE): packet generators. */

#include <stdlib.h>
#include <string.h>

#include "common.h"

/* The depth a bits_per_coded_sample value decodes as (33, 34, 36 and 40
 * are the greyscale forms of 1, 2, 4 and 8), or 0. */
static int depth_of(int bits)
{
    switch (bits) {
    case 1: case 2: case 4: case 8: case 16: case 24: case 32:
        return bits;
    case 33: case 34: case 36: case 40:
        return bits - 32;
    }
    return 0;
}

/* The stream counts in units. A unit is this many bytes of picture... */
static int unit_out(int depth)
{
    switch (depth) {
    case 1: case 2: return 16;
    case 4:         return 8;
    case 8:         return 4;
    case 16:        return 2;
    case 24:        return 3;
    }
    return 4;
}

/* ... and this many bytes of stream, in a run and per unit of a copy. */
static int unit_in(int depth)
{
    switch (depth) {
    case 1: case 16: return 2;
    case 24:         return 3;
    }
    return 4;
}

static int units_per_row(const Cfg *cfg, int depth)
{
    int bytes = cfg->w * (depth <= 8 ? 1 : depth / 8);
    return (bytes + unit_out(depth) - 1) / unit_out(depth);
}

static void config(Cfg *cfg)
{
    static const int depths[] = { 1, 2, 4, 8, 8, 16, 16, 24, 24, 32, 32, 33, 34, 36, 40 };

    if (rnd(40) == 0)
        cfg->bits = rnd(48);            /* mostly depths the decoder refuses */
    else
        cfg->bits = depths[rnd(sizeof(depths) / sizeof(depths[0]))];
}

/* opt[0]: 0 keeps the default discard_damaged_percentage (95), otherwise
 * the percentage plus one. */
static void options(Cfg *cfg)
{
    static const int pct[] = { 0, 1, 50, 94, 99, 100 };

    cfg->opt[0] = rnd(4) ? 0 : 1 + pct[rnd(6)];
}

static void setup(AVCodecContext *ctx, const Cfg *cfg)
{
    if (cfg->opt[0])
        ctx->discard_damaged_percentage = cfg->opt[0] - 1;
}

static void put_be16(Buf *b, int v)
{
    put(b, v >> 8);
    put(b, v);
}

static void put_random(Buf *b, int n)
{
    for (int i = 0; i < n; i++)
        put(b, rnd(256));
}

/* Chunk size and header word; with region != 0, also first line and line
 * count. The size is written by set_size once the packet is complete. */
static void put_header(Buf *b, int flags, int region, int start, int lines)
{
    put(b, 0); put(b, 0); put(b, 0); put(b, 0);
    put_be16(b, flags | (region ? 8 : 0));
    if (region) {
        put_be16(b, start);
        put_be16(b, rnd(4) ? 0 : rnd(65536));
        put_be16(b, lines);
        put_be16(b, rnd(4) ? 0 : rnd(65536));
    }
}

static void set_size(Buf *b, unsigned size)
{
    if (b->n >= 4) {
        b->d[0] = size >> 24;
        b->d[1] = size >> 16;
        b->d[2] = size >> 8;
        b->d[3] = size;
    }
}

/* One run code: -n, then one unit of pixels. n is 2 to 128 (-1 is the
 * end-of-line code). */
static void put_run(Buf *b, int depth, int n)
{
    put(b, -n);
    put_random(b, unit_in(depth));
}

/* One copy code: n, then n units of pixels. n is 1 to 127. */
static void put_copy(Buf *b, int depth, int n)
{
    put(b, n);
    put_random(b, n * unit_in(depth));
}

static void gen(Buf *b, const Cfg *cfg)
{
    int depth = depth_of(cfg->bits);
    int r = rnd(24), region, start = 0, lines = cfg->h, units;

    b->n = 0;
    if (r == 0 || !depth) {
        put_random(b, 1 + rnd(rnd(4) ? 64 : 2048));
        return;
    }
    if (r == 1) {                       /* too short to hold a picture */
        put_random(b, 1 + rnd(14));
        return;
    }

    region = rnd(3) == 0;
    if (region) {
        start = rnd(cfg->h + 1);
        lines = rnd(cfg->h - start + 1);
        if (rnd(8) == 0)
            lines = cfg->h - start;
        if (rnd(12) == 0)               /* now and then not inside the picture */
            *(rnd(2) ? &start : &lines) += 1 + rnd(3);
    }
    put_header(b, rnd(8) ? 0 : rnd(65536), region, start, lines);
    if (rnd(10) == 0)
        lines += rnd(3) - 1;

    units = units_per_row(cfg, depth);
    for (int y = 0; y < lines && b->n < MAX_PKT - 4096; y++) {
        int x = 0, first = 1;

        /* Every line starts with a skip. In 1-bit streams it is a byte
         * with the top bit set in front of the first code; in the others a
         * byte of its own, one more than the number of units skipped. */
        if (rnd(6) == 0)
            x = rnd(units + 2);
        if (depth != 1)
            put(b, rnd(50) ? x + 1 : rnd(256));

        while (b->n < MAX_PKT - 4096) {
            int left = units - x, n, skip = 0;

            if (left <= 0 && rnd(12))
                break;
            if (left <= 0)              /* past the end of the row */
                left = 1 + rnd(70);
            else if (rnd(12) == 0)
                left += 1 + rnd(70);
            if (left > 127)
                left = 127;
            n = 1 + rnd(rnd(3) ? left : (left < 6 ? left : 6));

            if (depth == 1) {
                if (first)
                    skip = 0x80 | (x & 0x7f);
                else if (rnd(5) == 0)
                    x += skip = 1 + rnd(left);
                put(b, skip);
            } else if (rnd(6) == 0) {
                put(b, 0);              /* skip code */
                put(b, n + 1);
                x += n;
                continue;
            }
            first = 0;
            if (depth == 1 && rnd(8) == 0) {
                put(b, 0xff);           /* 1-bit: only the skip */
                continue;
            }
            if (rnd(2) && n > 1)
                put_run(b, depth, n);
            else
                put_copy(b, depth, n);
            x += n;
            if (rnd(30) == 0)
                break;                  /* short row */
        }
        if (depth == 1) {
            if (first && rnd(20)) {     /* empty line */
                put(b, 0x80);
                put(b, 0xff);
            }
        } else if (rnd(40)) {
            put(b, 0xff);               /* end of line */
        }
    }
    if (rnd(4))
        put(b, 0), put(b, 0);           /* 1-bit: end of picture; others: unused */

    r = rnd(20);
    set_size(b, r == 0 ? rnd64() : r == 1 ? b->n + rnd(40 * b->n + 2) : b->n);
}

/* One frame that takes a single decoder path for every pixel. Kinds:
 * runD, copyD (D = 1, 2, 4, 8, 16, 24, 32), with "-short" for pieces of
 * two or three units instead of the longest a code can hold, and skipD: a
 * region header and a skip in every line (the picture does not change). */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    AVPacket *pkt;
    int depth = atoi(kind + strcspn(kind, "0123456789"));
    int is_copy = !strncmp(kind, "copy", 4), is_run = !strncmp(kind, "run", 3);
    int is_skip = !strncmp(kind, "skip", 4);
    int piece = strstr(kind, "-short") ? (is_run ? 2 : 3) : 127;
    int units;

    if (depth_of(depth) != depth || (!is_copy && !is_run && !is_skip))
        return NULL;
    cfg->bits = depth;
    units = units_per_row(cfg, depth);
    b.n = 0;
    put_header(&b, 0, is_skip, 0, cfg->h);
    for (int y = 0; y < cfg->h; y++) {
        if (is_skip) {
            if (depth == 1) {
                put(&b, 0x80); put(&b, 0xff);
            } else {
                put(&b, 1); put(&b, 0xff);
            }
            continue;
        }
        if (depth != 1)
            put(&b, 1);
        for (int x = 0; x < units;) {
            int n = units - x < piece ? units - x : piece;
            if (depth == 1)
                put(&b, x ? 0 : 0x80);
            if (is_copy || n < 2)
                put_copy(&b, depth, n);
            else
                put_run(&b, depth, n);
            x += n;
        }
        if (depth != 1)
            put(&b, 0xff);
    }
    put(&b, 0); put(&b, 0);
    set_size(&b, b.n);
    if (b.n >= MAX_PKT)
        return NULL;
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = { 0x00, 0x01, 0x02, 0x08, 0x7f, 0x80, 0x81, 0xfe, 0xff };

const CodecOps ops_qtrle = {
    .name              = "qtrle",
    .c_name            = "qtrle",
    .rs_name           = "qtrle_rs",
    .id                = AV_CODEC_ID_QTRLE,
    .interesting       = interesting,
    .nb_interesting    = sizeof(interesting),
    .palette_side_data = 1,
    .config            = config,
    .options           = options,
    .setup             = setup,
    .gen               = gen,
    .synth_kinds       = "run8 run8-short copy8 copy8-short run16 run16-short copy16 copy16-short "
                         "run24 run24-short copy24 copy24-short run32 run32-short copy32 copy32-short "
                         "run1 copy1 run2 copy2 run4 copy4 skip24",
    .synth             = synth,
};
