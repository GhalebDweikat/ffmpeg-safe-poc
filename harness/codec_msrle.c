/* Microsoft RLE: packet generators. */

#include <stdlib.h>
#include <string.h>

#include "common.h"

static void config(Cfg *cfg)
{
    static const int depths[] = { 8, 8, 8, 4, 4, 24, 24, 1 };
    int r = rnd(40);

    if (r == 0)
        cfg->bits = rnd(40);            /* mostly depths the decoder refuses */
    else
        cfg->bits = depths[rnd(8)];

    /* Stream header palette: none, a full one, or an odd-sized one. */
    r = rnd(4);
    cfg->extradata_size = r == 0 ? 0 : r == 1 ? 1024 : rnd(1025);
    for (int i = 0; i < cfg->extradata_size; i++)
        cfg->extradata[i] = rnd(256);
}

static int raw_size(const Cfg *cfg)
{
    return cfg->h * (((cfg->w * cfg->bits + 31) / 32) * 4);
}

/* Bytes one coded pixel value takes in a run, and n pixels in a copy. */
static void put_run(Buf *b, int bits, int n)
{
    put(b, n);
    for (int i = 0; i < (bits == 24 ? 3 : 1); i++)
        put(b, rnd(256));
}

static void put_copy(Buf *b, int bits, int n)
{
    int bytes = bits == 24 ? 3 * n : bits == 4 ? (n + 1) / 2 : n;
    put(b, 0);
    put(b, n);
    for (int i = 0; i < bytes; i++)
        put(b, rnd(256));
    if (bits != 24 && (bytes & 1))
        put(b, 0);
}

static void gen(Buf *b, const Cfg *cfg)
{
    int r = rnd(16), lines = cfg->h;

    b->n = 0;
    if (r == 0 || (cfg->bits != 4 && cfg->bits != 8 && cfg->bits != 24 && r < 8)) {
        int n = 1 + rnd(rnd(4) ? 64 : 2048);
        for (int i = 0; i < n; i++)
            put(b, rnd(256));
        return;
    }
    if (r == 1 || (cfg->bits != 4 && cfg->bits != 8 && cfg->bits != 24)) {
        /* Exactly the size of a raw picture. */
        int n = raw_size(cfg);
        if (n > MAX_PKT)
            n = MAX_PKT;
        for (int i = 0; i < n; i++)
            put(b, rnd(256));
        return;
    }

    if (rnd(8) == 0)
        lines += rnd(3) - 1;
    for (int y = 0; y < lines && b->n < MAX_PKT - 2048; y++) {
        int x = 0;
        while (x < cfg->w && b->n < MAX_PKT - 2048) {
            int left = cfg->w - x, n;
            /* Now and then run past the end of the row. */
            if (rnd(12) == 0)
                left += 1 + rnd(70);
            if (left > 255)
                left = 255;
            n = 1 + rnd(rnd(3) ? left : (left < 8 ? left : 8));
            switch (rnd(8)) {
            case 0:                     /* skip right and up */
                put(b, 0); put(b, 2);
                put(b, n); put(b, rnd(6) ? 0 : rnd(4));
                break;
            case 1: case 2: case 3:
                if (n < 3)
                    n = 3;
                put_copy(b, cfg->bits, n);
                break;
            default:
                put_run(b, cfg->bits, n);
                break;
            }
            x += n;
            if (rnd(30) == 0)
                break;                  /* short row */
        }
        if (rnd(40))
            put(b, 0), put(b, 0);       /* end of line */
    }
    if (rnd(10)) {
        put(b, 0); put(b, 1);           /* end of picture */
    }
}

/* One frame that takes a single decoder path for every pixel. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    AVPacket *pkt;
    int bits = !strncmp(kind, "raw", 3) ? atoi(kind + 3) : atoi(kind + strcspn(kind, "0123456789"));
    int is_copy = !strncmp(kind, "copy", 4), is_run = !strncmp(kind, "run", 3);
    int piece = strstr(kind, "-short") ? 3 : bits == 24 ? 85 : 254;

    if (bits != 4 && bits != 8 && bits != 24)
        return NULL;
    cfg->bits = bits;
    b.n = 0;
    if (!strncmp(kind, "raw", 3)) {
        int n = raw_size(cfg);
        for (int i = 0; i < n; i++)
            put(&b, rnd(256));
    } else if (!strncmp(kind, "empty", 5)) {
        put(&b, 0); put(&b, 1);
    } else if (is_copy || is_run) {
        for (int y = 0; y < cfg->h; y++) {
            for (int x = 0; x < cfg->w;) {
                int n = cfg->w - x < piece ? cfg->w - x : piece;
                if (is_copy && n < 3) {  /* copies shorter than 3 do not exist */
                    put_run(&b, bits, n);
                } else if (is_copy) {
                    put_copy(&b, bits, n);
                } else {
                    put_run(&b, bits, n);
                }
                x += n;
            }
            /* The 4-bit decoder stops by itself after the last row. */
            if (bits != 4 || y + 1 < cfg->h)
                put(&b, 0), put(&b, 0);
        }
        put(&b, 0); put(&b, 1);
    } else {
        return NULL;
    }
    if (b.n >= MAX_PKT)
        return NULL;
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = { 0x00, 0x01, 0x02, 0x03, 0x04, 0x7f, 0x80, 0xfe, 0xff };

const CodecOps ops_msrle = {
    .name              = "msrle",
    .c_name            = "msrle",
    .rs_name           = "msrle_rs",
    .id                = AV_CODEC_ID_MSRLE,
    .interesting       = interesting,
    .nb_interesting    = sizeof(interesting),
    .palette_side_data = 1,
    .config            = config,
    .gen               = gen,
    .synth_kinds       = "run8 run8-short copy8 copy8-short empty8 raw8 run4 copy4 run24 copy24",
    .synth             = synth,
};
