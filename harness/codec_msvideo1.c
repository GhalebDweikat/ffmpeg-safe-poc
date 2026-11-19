/* Microsoft Video 1: packet generators and the direct-call check. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "common.h"

int msvideo1_rs_check(size_t pkt_size, int width, int height);
int msvideo1_rs_decode(const uint8_t *pkt, size_t pkt_size,
                       uint8_t *frame, size_t frame_size, size_t stride,
                       int width, int height, int palette);

#define PAD 0xA5

static void config(Cfg *cfg)
{
    int r = rnd(20);

    /* The decoder refuses pictures under 4x4. Keep a few of those, and
     * move the rest of the harness's small sizes up. */
    if (rnd(8)) {
        if (cfg->w < 4)
            cfg->w += 4;
        if (cfg->h < 4)
            cfg->h += 4;
    }
    /* Now and then a picture of more than 1023 blocks, so that the longest
     * skip code does not cover it and the minimum packet size is not 0. */
    if (rnd(24) == 0) {
        cfg->w = 4 * (33 + rnd(100)) + rnd(4);
        cfg->h = 4 * (33 + rnd(60)) + rnd(4);
    }

    /* 8 is palette video; anything else is decoded as 16-bit. */
    cfg->bits = r < 9 ? 8 : r < 18 ? 16 : rnd(40);

    /* Stream header palette: none, a full one, or one too short to use. */
    r = rnd(4);
    cfg->extradata_size = r == 0 ? 0 : r == 1 ? 1024 : rnd(1025);
    for (int i = 0; i < cfg->extradata_size; i++)
        cfg->extradata[i] = rnd(256);
}

static void le16(Buf *b, int v)
{
    put(b, v); put(b, v >> 8);
}

/* Sixteen flag bits whose high byte is below 0x80. */
static int flags15(void)
{
    switch (rnd(12)) {
    case 0:  return 0;
    case 1:  return 0x7fff;
    default: return rnd(0x8000);
    }
}

enum { SKIP, FILL, TWO, EIGHT };

/* One block code. Returns the number of blocks it covers (0: all the rest). */
static int gen_block(Buf *b, int pal, int type, int skip)
{
    int hi;

    switch (type) {
    case SKIP:
        le16(b, 0x8400 | skip);
        return skip;
    case FILL:
        /* High byte 0x80 to 0x8f (palette) or 0x80 to 0xff (16-bit), but
         * not 0x84 to 0x87, which is a skip. */
        hi = pal ? 0x80 | rnd(16) : 0x80 | rnd(128);
        if ((hi & 0xfc) == 0x84)
            hi ^= 0x08;
        put(b, rnd(256)); put(b, hi);
        return 1;
    case TWO:
        le16(b, flags15());
        if (pal) {
            put(b, rnd(256)); put(b, rnd(256));
        } else {
            le16(b, rnd(0x8000)); le16(b, rnd(0x10000));
        }
        return 1;
    default:
        if (pal) {
            put(b, rnd(256)); put(b, 0x90 + rnd(0x70));
            for (int i = 0; i < 8; i++)
                put(b, rnd(256));
        } else {
            le16(b, flags15());
            le16(b, 0x8000 | rnd(0x8000));
            for (int i = 0; i < 7; i++)
                le16(b, rnd(0x10000));
        }
        return 1;
    }
}

static void gen(Buf *b, const Cfg *cfg)
{
    int pal = cfg->bits == 8;
    int total = (cfg->w / 4) * (cfg->h / 4), target = total, done = 0;
    int r = rnd(16);

    b->n = 0;
    if (r == 0) {
        int n = 1 + rnd(rnd(4) ? 64 : 2048);
        for (int i = 0; i < n; i++)
            put(b, rnd(256));
        return;
    }
    if (r == 1)
        target = rnd(total + 1);                /* the packet runs out */
    else if (r == 2)
        target = total + 1 + rnd(8);            /* codes after the last block */

    while (done < target && b->n < MAX_PKT - 64) {
        int n;
        switch (rnd(10)) {
        case 0: case 1:
            /* Short runs mostly; sometimes up to the longest (1023), and
             * sometimes a count of 0, which skips the rest of the frame. */
            n = rnd(8) ? 1 + rnd(12) : rnd(4) ? rnd(1024) : 0;
            n = gen_block(b, pal, SKIP, n);
            done += n ? n : total;
            break;
        case 2: case 3:
            done += gen_block(b, pal, FILL, 0);
            break;
        case 4: case 5: case 6:
            done += gen_block(b, pal, TWO, 0);
            break;
        default:
            done += gen_block(b, pal, EIGHT, 0);
            break;
        }
    }
    if (rnd(3) == 0)
        le16(b, 0);                             /* the encoder's end marker */
}

/* Direct call: exactly the visible picture, zeroed like a fresh FFmpeg
 * frame and kept from packet to packet as the decoder keeps its frame,
 * with marker bytes in the padding between rows. */
typedef struct Direct {
    int w, h, pal;
    size_t row, stride, size;
    uint8_t *px;
} Direct;

static void *direct_open(const Cfg *cfg)
{
    Direct *d = malloc(sizeof(*d));
    d->w      = cfg->w;
    d->h      = cfg->h;
    d->pal    = cfg->bits == 8;
    d->row    = (size_t)cfg->w * (d->pal ? 1 : 2);
    d->stride = d->row + (d->pal ? rnd(5) : 2 * rnd(3));
    d->size   = d->stride * (cfg->h - 1) + d->row;
    d->px     = malloc(d->size);
    memset(d->px, PAD, d->size);
    for (int y = 0; y < cfg->h; y++)
        memset(d->px + y * d->stride, 0, d->row);
    return d;
}

static int direct_step(void *st, const Buf *b, const AVFrame *rs_frame,
                       int rs_got_frame, char *why, size_t why_size)
{
    Direct *d = st;
    int status = 2;

    if (msvideo1_rs_check(b->n, d->w, d->h) == 1) {
        status = msvideo1_rs_decode(b->d, b->n, d->px, d->size, d->stride,
                                    d->w, d->h, d->pal);
        if (status != 0 && status != 1) {
            snprintf(why, why_size, "bad status %d", status);
            return -1;
        }
    }
    /* The direct call must agree with the libavcodec path ... */
    if ((status != 2) != rs_got_frame) {
        snprintf(why, why_size, "status %d but frame=%d", status, rs_got_frame);
        return -1;
    }
    if (rs_got_frame)
        for (int y = 0; y < d->h; y++)
            if (memcmp(d->px + y * d->stride,
                       rs_frame->data[0] + y * rs_frame->linesize[0], d->row)) {
                snprintf(why, why_size, "pixels differ in row %d", y);
                return -1;
            }
    /* ... and must leave the row padding alone. */
    for (int y = 0; y + 1 < d->h; y++)
        for (size_t x = d->row; x < d->stride; x++)
            if (d->px[y * d->stride + x] != PAD) {
                snprintf(why, why_size, "padding written in row %d", y);
                return -1;
            }
    return status;
}

static void direct_close(void *st)
{
    Direct *d = st;
    free(d->px);
    free(d);
}

/* One frame whose blocks all take the same decoder path ("mixed": a mix
 * like an encoded clip's). KIND-pal is palette video, KIND-rgb 16-bit. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    AVPacket *pkt;
    int total = (cfg->w / 4) * (cfg->h / 4), done = 0, pal;
    const char *dash = strchr(kind, '-');
    size_t len = dash ? (size_t)(dash - kind) : 0;
    int type;

    if (!dash || (strcmp(dash, "-rgb") && strcmp(dash, "-pal")))
        return NULL;
    pal = !strcmp(dash, "-pal");
    cfg->bits = pal ? 8 : 16;

    if (len == 5 && !strncmp(kind, "skip1", 5))
        type = SKIP;                            /* a skip code per block */
    else if (len == 7 && !strncmp(kind, "skipmax", 7))
        type = SKIP + 16;                       /* the longest skip codes */
    else if (len == 4 && !strncmp(kind, "fill", 4))
        type = FILL;
    else if (len == 3 && !strncmp(kind, "two", 3))
        type = TWO;
    else if (len == 5 && !strncmp(kind, "eight", 5))
        type = EIGHT;
    else if (len == 5 && !strncmp(kind, "mixed", 5))
        type = -1;
    else
        return NULL;

    b.n = 0;
    while (done < total && b.n < MAX_PKT - 64) {
        int left = total - done, t = type, n = 1;
        if (type == SKIP + 16) {
            t = SKIP;
            n = left < 1023 ? left : 1023;
        } else if (type == -1) {
            int r = rnd(20);
            t = r < 5 ? SKIP : r < 10 ? FILL : r < 17 ? TWO : EIGHT;
            n = 1 + rnd(6);
            if (n > left)
                n = left;
        }
        done += gen_block(&b, pal, t, n);
    }
    le16(&b, 0);
    if (b.n >= MAX_PKT - 64)
        return NULL;
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = {
    0x00, 0x01, 0x02, 0x7f, 0x80, 0x83, 0x84, 0x85, 0x87, 0x88, 0x8f, 0x90, 0xff,
};

static const char *const status_names[] = { "finished", "ran_out", "too_small" };

const CodecOps ops_msvideo1 = {
    .name              = "msvideo1",
    .c_name            = "msvideo1",
    .rs_name           = "msvideo1_rs",
    .id                = AV_CODEC_ID_MSVIDEO1,
    .interesting       = interesting,
    .nb_interesting    = sizeof(interesting),
    .palette_side_data = 1,
    .config            = config,
    .gen               = gen,
    .direct_open       = direct_open,
    .direct_step       = direct_step,
    .direct_close      = direct_close,
    .status_names      = status_names,
    .nb_status         = 3,
    .synth_kinds       = "skip1-pal skipmax-pal fill-pal two-pal eight-pal mixed-pal "
                         "skip1-rgb skipmax-rgb fill-rgb two-rgb eight-rgb mixed-rgb",
    .synth             = synth,
};
