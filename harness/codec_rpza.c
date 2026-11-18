/* Apple RPZA: packet generators and the direct-call check. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "common.h"

int rpza_rs_decode(const uint8_t *pkt, size_t pkt_size,
                   uint8_t *frame, size_t frame_size, size_t stride,
                   int width, int height, uint32_t *opcode);

#define PAD 0xA5

static void config(Cfg *cfg)
{
    (void)cfg;
}

/* A syntactically valid opcode stream for a w x h frame, sometimes with too
 * many or too few blocks. */
static void gen_structured(Buf *b, int w, int h)
{
    int total  = ((w + 3) / 4) * ((h + 3) / 4);
    int target = total;
    int done   = 0;
    int r      = rnd(10);

    if (r == 0)
        target = total + 1 + rnd(40);
    else if (r == 1)
        target = rnd(total + 1);

    b->n = 0;
    put(b, 0xe1); put(b, 0); put(b, 0); put(b, 0);

    while (done < target && b->n < MAX_PKT - 200) {
        int n = 1 + rnd(32), i;
        if (rnd(4) == 0)
            n = 1 + rnd(4);
        switch (rnd(10)) {
        case 0: case 1:                 /* skip */
            put(b, 0x80 | (n - 1));
            done += n;
            break;
        case 2: case 3:                 /* one colour */
            put(b, 0xa0 | (n - 1));
            put(b, rnd(256)); put(b, rnd(256));
            done += n;
            break;
        case 4: case 5: case 6:         /* four colours */
            put(b, 0xc0 | (n - 1));
            for (i = 0; i < 4 + 4 * n; i++)
                put(b, rnd(256));
            done += n;
            break;
        case 7:                         /* four colours, short form */
            put(b, rnd(128)); put(b, rnd(256));
            put(b, 0x80 | rnd(128)); put(b, rnd(256));
            for (i = 0; i < 4; i++)
                put(b, rnd(256));
            done += 1;
            break;
        default:                        /* sixteen colours */
            put(b, rnd(128)); put(b, rnd(256));
            put(b, rnd(128)); put(b, rnd(256));
            for (i = 0; i < 28; i++)
                put(b, rnd(256));
            done += 1;
            break;
        }
    }

    if (rnd(10)) {
        b->d[1] = b->n >> 16; b->d[2] = b->n >> 8; b->d[3] = b->n;
    } else {
        b->d[1] = rnd(256); b->d[2] = rnd(256); b->d[3] = rnd(256);
    }
}

static void gen(Buf *b, const Cfg *cfg)
{
    if (rnd(8)) {
        gen_structured(b, cfg->w, cfg->h);
    } else {
        int n = 1 + rnd(rnd(4) ? 64 : 2048);
        b->n = 0;
        for (int i = 0; i < n; i++)
            put(b, rnd(256));
        if (rnd(2))
            b->d[0] = 0xe1;
    }
}

/* Direct call: exactly the visible image, zeroed like a fresh FFmpeg frame,
 * with marker bytes in the padding between rows. */
typedef struct Direct {
    int w, h;
    size_t stride, size;
    uint8_t *px;
} Direct;

static void *direct_open(const Cfg *cfg)
{
    Direct *d = malloc(sizeof(*d));
    d->w      = cfg->w;
    d->h      = cfg->h;
    d->stride = 2 * (size_t)cfg->w + 2 * rnd(5);
    d->size   = d->stride * (cfg->h - 1) + 2 * (size_t)cfg->w;
    d->px     = malloc(d->size);
    memset(d->px, PAD, d->size);
    for (int y = 0; y < cfg->h; y++)
        memset(d->px + y * d->stride, 0, 2 * (size_t)cfg->w);
    return d;
}

static int direct_step(void *st, const Buf *b, const AVFrame *rs_frame,
                       int rs_got_frame, char *why, size_t why_size)
{
    Direct *d = st;
    uint32_t op = 0;
    int status = rpza_rs_decode(b->d, b->n, d->px, d->size, d->stride,
                                d->w, d->h, &op);

    if (status > 0 || status < -5) {
        snprintf(why, why_size, "bad status %d", status);
        return -1;
    }
    /* The direct call must agree with the libavcodec path ... */
    if ((status == 0) != rs_got_frame) {
        snprintf(why, why_size, "status %d but frame=%d", status, rs_got_frame);
        return -1;
    }
    if (rs_got_frame)
        for (int y = 0; y < d->h; y++)
            if (memcmp(d->px + y * d->stride,
                       rs_frame->data[0] + y * rs_frame->linesize[0],
                       2 * (size_t)d->w)) {
                snprintf(why, why_size, "pixels differ in row %d", y);
                return -1;
            }
    /* ... and must leave the row padding alone. */
    for (int y = 0; y + 1 < d->h; y++)
        for (size_t x = 2 * (size_t)d->w; x < d->stride; x++)
            if (d->px[y * d->stride + x] != PAD) {
                snprintf(why, why_size, "padding written in row %d", y);
                return -1;
            }
    return -status;
}

static void direct_close(void *st)
{
    Direct *d = st;
    free(d->px);
    free(d);
}

/* One frame in which every block is coded with the same opcode type. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    int total = ((cfg->w + 3) / 4) * ((cfg->h + 3) / 4), done = 0, n = 0, i;
    uint8_t *d = malloc(40 * (size_t)total + 64);
    AVPacket *pkt = av_packet_alloc();

    d[n++] = 0xe1; n += 3;
    while (done < total) {
        int run = total - done < 32 ? total - done : 32;
        if (!strcmp(kind, "skip32")) {
            d[n++] = 0x80 | (run - 1);
        } else if (!strcmp(kind, "fill1")) {
            run = 1;
            d[n++] = 0xa0; d[n++] = rnd(128); d[n++] = rnd(256);
        } else if (!strcmp(kind, "fill32")) {
            d[n++] = 0xa0 | (run - 1); d[n++] = rnd(128); d[n++] = rnd(256);
        } else if (!strcmp(kind, "four1")) {
            run = 1;
            d[n++] = rnd(128); d[n++] = rnd(256);
            d[n++] = 0x80 | rnd(128); d[n++] = rnd(256);
            for (i = 0; i < 4; i++) d[n++] = rnd(256);
        } else if (!strcmp(kind, "fourc1") || !strcmp(kind, "four32")) {
            if (kind[4] == 'c') run = 1;
            d[n++] = 0xc0 | (run - 1);
            for (i = 0; i < 4 + 4 * run; i++) d[n++] = rnd(256);
        } else if (!strcmp(kind, "sixteen")) {
            run = 1;
            d[n++] = rnd(128); d[n++] = rnd(256);
            d[n++] = rnd(128); d[n++] = rnd(256);
            for (i = 0; i < 28; i++) d[n++] = rnd(256);
        } else {
            free(d);
            av_packet_free(&pkt);
            return NULL;
        }
        done += run;
    }
    d[1] = n >> 16; d[2] = n >> 8; d[3] = n;
    av_new_packet(pkt, n);
    memcpy(pkt->data, d, n);
    free(d);
    return pkt;
}

static const uint8_t interesting[] = {
    0x00, 0x01, 0x1f, 0x20, 0x3f, 0x7f, 0x80, 0x9f, 0xa0, 0xbf,
    0xc0, 0xdf, 0xe0, 0xe1, 0xff,
};

static const char *const status_names[] = {
    "ok", "too_small", "four_trunc", "sixteen_trunc", "no_blocks", "unknown_op",
};

const CodecOps ops_rpza = {
    .name           = "rpza",
    .c_name         = "rpza",
    .rs_name        = "rpza_rs",
    .id             = AV_CODEC_ID_RPZA,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .direct_open    = direct_open,
    .direct_step    = direct_step,
    .direct_close   = direct_close,
    .status_names   = status_names,
    .nb_status      = 6,
    .synth_kinds    = "skip32 fill1 fill32 four1 fourc1 four32 sixteen",
    .synth          = synth,
};
