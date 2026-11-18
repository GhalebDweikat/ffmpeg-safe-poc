/* v210 (uncompressed 10-bit 4:2:2): packet generators. */

#include <stdlib.h>
#include <string.h>

#include "libavutil/macros.h"

#include "common.h"

/* Test hook in the Rust library: 0 forces the portable decoder. */
void v210_rs_set_simd(int enabled);

static int v210_stride(int width, int align)
{
    return ((width + align - 1) / align) * align * 8 / 3;
}

static void config(Cfg *cfg)
{
    switch (rnd(8)) {
    case 0:  cfg->tag = MKTAG('b', 'x', 'y', '2'); break;   /* unpadded rows */
    case 1:  cfg->tag = MKTAG('C', '2', '1', '0'); break;   /* optional INFO header */
    default: cfg->tag = MKTAG('v', '2', '1', '0'); break;
    }
    /* With fewer than four rows the C decoder returns the frame without
     * decoding it (its slice count becomes zero), so there is nothing to
     * compare against. */
    if (cfg->h < 4 && !getenv("V210_SMALL_HEIGHTS"))
        cfg->h += 4;
    /* A quarter of the cases run the Rust decoder without AVX2. */
    v210_rs_set_simd(rnd(4) != 0);
}

static void fill(Buf *b, int n)
{
    if (n > MAX_PKT)
        n = MAX_PKT;
    while (b->n + 8 <= n) {
        uint64_t r = rnd64();
        memcpy(b->d + b->n, &r, 8);
        b->n += 8;
    }
    while (b->n < n)
        put(b, rnd(256));
}

static void gen(Buf *b, const Cfg *cfg)
{
    static const int aligns[] = { 48, 48, 48, 48, 24, 12, 6 };
    int size = v210_stride(cfg->w, aligns[rnd(7)]) * cfg->h;

    b->n = 0;
    if (cfg->tag == MKTAG('b', 'x', 'y', '2') && rnd(3))
        size = v210_stride(cfg->w * cfg->h, 6);
    switch (rnd(12)) {
    case 0: size += 1 + rnd(200);               break;
    case 1: size -= 1 + rnd(size > 1 ? size : 1); break;
    case 2: size  = rnd(4096);                  break;
    }
    if (size < 1)
        size = 1;
    if (cfg->tag == MKTAG('C', '2', '1', '0') && rnd(2)) {
        /* A 64-byte header, with or without room for the picture after it. */
        memcpy(b->d, "INFO", 4);
        b->n = 4;
        fill(b, 64);
        if (rnd(4))
            size += 64;
    }
    fill(b, size);
}

/* One frame of random samples. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    AVPacket *pkt;

    if (strcmp(kind, "random"))
        return NULL;
    cfg->tag = MKTAG('v', '2', '1', '0');
    b.n = 0;
    fill(&b, v210_stride(cfg->w, 48) * cfg->h);
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = { 0x00, 0xff, 0x3f, 0xc0, 'I', 'N', 'F', 'O' };

const CodecOps ops_v210 = {
    .name           = "v210",
    .c_name         = "v210",
    .rs_name        = "v210_rs",
    .id             = AV_CODEC_ID_V210,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .synth_kinds    = "random",
    .synth          = synth,
};
