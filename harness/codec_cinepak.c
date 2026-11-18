/* Cinepak: packet generators. */

#include <stdlib.h>
#include <string.h>

#include "common.h"

/* Extra bytes after the frame header, as in Sega FILM files. Chosen per
 * fuzz case, because the decoder decides it once, on the first frame. */
static int film_skip;

static void config(Cfg *cfg)
{
    int r = rnd(12);
    cfg->bits = r < 4 ? 8 : r == 4 ? rnd(40) : 24;
    r = rnd(10);
    film_skip = r == 0 ? 2 : r == 1 ? 6 : 0;
}

static void be16(Buf *b, int v)
{
    put(b, v >> 8); put(b, v);
}

static void patch24(Buf *b, int at, int v)
{
    b->d[at] = v >> 16; b->d[at + 1] = v >> 8; b->d[at + 2] = v;
}

/* Emits flag words exactly when the decoder will ask for one. */
typedef struct FlagGen {
    uint32_t flag, mask;
    int mode;                   /* 0-2: about 25/50/75% set; 3: none; 4: all */
} FlagGen;

static int next_flag(Buf *b, FlagGen *f)
{
    f->mask >>= 1;
    if (!f->mask) {
        f->flag = rnd64();
        switch (f->mode) {
        case 0: f->flag &= rnd64();  break;
        case 2: f->flag |= rnd64();  break;
        case 3: f->flag  = 0;        break;
        case 4: f->flag  = ~0u;      break;
        }
        put(b, f->flag >> 24); put(b, f->flag >> 16); put(b, f->flag >> 8); put(b, f->flag);
        f->mask = 0x80000000;
    }
    return !!(f->flag & f->mask);
}

static void gen_codebook(Buf *b, int id, int entries, int mode)
{
    int n = (id & 4) ? 4 : 6, start = b->n;
    FlagGen f = { 0, 0, mode };

    put(b, id); put(b, 0); put(b, 0); put(b, 0);
    for (int i = 0; i < entries; i++) {
        if ((id & 1) && !next_flag(b, &f))
            continue;
        for (int k = 0; k < n; k++)
            put(b, rnd(256));
    }
    patch24(b, start + 1, b->n - start);
}

static void gen_vectors(Buf *b, int id, int blocks, int mode)
{
    int start = b->n;
    FlagGen f = { 0, 0, mode };

    put(b, id); put(b, 0); put(b, 0); put(b, 0);
    for (int i = 0; i < blocks && b->n < MAX_PKT - 64; i++) {
        if ((id & 1) && !next_flag(b, &f))
            continue;
        if ((id & 2) || !next_flag(b, &f)) {
            put(b, rnd(256));                           /* one V1 index */
        } else {
            for (int k = 0; k < 4; k++)                 /* four V4 indices */
                put(b, rnd(256));
        }
    }
    patch24(b, start + 1, b->n - start);
}

static void frame_header(Buf *b, int flags, int w, int h, int nstrips)
{
    static const uint8_t film6[6] = { 0xfe, 0x00, 0x00, 0x06, 0x00, 0x00 };

    b->n = 0;
    put(b, flags); put(b, 0); put(b, 0); put(b, 0);
    be16(b, w); be16(b, h); be16(b, nstrips);
    for (int i = 0; i < film_skip; i++)
        put(b, film_skip == 6 ? film6[i] : rnd(256));
}

/* The size the header claims. With extra FILM bytes it must differ from
 * the packet size and not divide it, which is how the decoder detects them. */
static void frame_size(Buf *b)
{
    patch24(b, 1, film_skip && b->n > 2 * film_skip ? b->n - film_skip : b->n);
}

static void strip_header(Buf *b, int id, int x1, int y1, int x2, int y2)
{
    put(b, id); put(b, 0); put(b, 0); put(b, 0);
    be16(b, y1); be16(b, x1); be16(b, y2); be16(b, x2);
}

static void gen(Buf *b, const Cfg *cfg)
{
    static const uint8_t v4_ids[] = { 0x20, 0x21, 0x24, 0x25 };
    static const uint8_t v1_ids[] = { 0x22, 0x23, 0x26, 0x27 };
    int W = (cfg->w + 3) & ~3, H = (cfg->h + 3) & ~3;
    int nstrips = 1 + rnd(3), y = 0;

    if (rnd(16) == 0) {
        int n = 1 + rnd(rnd(4) ? 64 : 2048);
        b->n = 0;
        for (int i = 0; i < n; i++)
            put(b, rnd(256));
        return;
    }
    if (rnd(20) == 0)
        nstrips = rnd(40);

    frame_header(b, rnd(2), cfg->w, cfg->h, nstrips);
    for (int s = 0; s < nstrips && b->n < MAX_PKT - (1 << 17); s++) {
        int start = b->n;
        int sh = ((H - y) / (nstrips - s)) & ~3;
        int x1 = 0, x2 = W, y1 = 0, y2;

        if (s == nstrips - 1 || sh == 0)
            sh = H - y > 0 ? H - y : 4;
        y2 = sh;                        /* y1 of 0: relative to the last strip */
        switch (rnd(24)) {
        case 0: x1 = rnd(4);                    break;  /* last block sticks out */
        case 1: x1 = rnd(W + 1);                break;
        case 2: x2 = W - rnd(4);                break;
        case 3: x2 = rnd(W + 6);                break;
        case 4: y1 = y ? y : 1; y2 = y1 + sh;   break;  /* absolute */
        case 5: y1 = rnd(H + 2); y2 = rnd(H + 6); break;
        case 6: y2 = sh + rnd(4);               break;
        }

        strip_header(b, rnd(8) ? 0x10 + rnd(2) : rnd(256), x1, y1, x2, y2);
        if (rnd(3))
            gen_codebook(b, v4_ids[rnd(4)], rnd(4) ? 256 : rnd(258), rnd(3));
        if (rnd(3))
            gen_codebook(b, v1_ids[rnd(4)], rnd(4) ? 256 : rnd(258), rnd(3));
        if (rnd(20) == 0) {             /* a chunk type the decoder skips */
            int n = rnd(12);
            put(b, rnd(256)); put(b, 0); put(b, 0); put(b, 4 + n);
            while (n--)
                put(b, rnd(256));
        }
        if (rnd(15)) {
            int bw = x2 > x1 ? (x2 - x1 + 3) / 4 : 0;
            int bh = y1 ? (y2 > y1 ? (y2 - y1 + 3) / 4 : 0) : (y2 + 3) / 4;
            gen_vectors(b, 0x30 + rnd(3), bw * bh, rnd(3));
        }
        patch24(b, start + 1, b->n - start);
        y += sh;
    }
    frame_size(b);
    if (rnd(15) == 0)
        patch24(b, 1, rnd(1 << (4 + rnd(20))));
}

/* One frame whose blocks all take the same decoder path. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    AVPacket *pkt;
    int W = (cfg->w + 3) & ~3, H = (cfg->h + 3) & ~3;
    int blocks = (W / 4) * (H / 4), start;
    const char *dash = strchr(kind, '-');
    size_t len = dash ? (size_t)(dash - kind) : 0;

    if (!dash || (strcmp(dash, "-rgb") && strcmp(dash, "-pal")))
        return NULL;
    cfg->bits = !strcmp(dash, "-pal") ? 8 : 24;
    film_skip = 0;

    if (len == 9 && !strncmp(kind, "codebooks", 9)) {
        /* Many short strips, each inheriting and then partly rewriting both
         * codebooks; no blocks coded. */
        int nstrips = H / 4 < 32 ? H / 4 : 32;
        frame_header(&b, 0, cfg->w, cfg->h, nstrips);
        for (int s = 0; s < nstrips; s++) {
            start = b.n;
            strip_header(&b, 0x11, 0, 0, W, 4);
            gen_codebook(&b, 0x21, 256, 1);
            gen_codebook(&b, 0x23, 256, 1);
            gen_vectors(&b, 0x31, W / 4, 3);
            patch24(&b, start + 1, b.n - start);
        }
    } else {
        frame_header(&b, 0, cfg->w, cfg->h, 1);
        start = b.n;
        strip_header(&b, 0x10, 0, 0, W, H);
        gen_codebook(&b, 0x20, 256, 1);
        gen_codebook(&b, 0x22, 256, 1);
        if (len == 2 && !strncmp(kind, "v1", 2))
            gen_vectors(&b, 0x32, blocks, 1);
        else if (len == 2 && !strncmp(kind, "v4", 2))
            gen_vectors(&b, 0x30, blocks, 4);
        else if (len == 4 && !strncmp(kind, "skip", 4))
            gen_vectors(&b, 0x31, blocks, 3);
        else if (len == 5 && !strncmp(kind, "mixed", 5))
            gen_vectors(&b, 0x31, blocks, 1);
        else
            return NULL;
        patch24(&b, start + 1, b.n - start);
    }
    frame_size(&b);
    if (b.n >= MAX_PKT - 64)
        return NULL;
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = {
    0x00, 0x01, 0x02, 0x04, 0x0c, 0x10, 0x11, 0x20, 0x21, 0x22, 0x23, 0x24,
    0x25, 0x26, 0x27, 0x30, 0x31, 0x32, 0x7f, 0x80, 0xfe, 0xff,
};

const CodecOps ops_cinepak = {
    .name              = "cinepak",
    .c_name            = "cinepak",
    .rs_name           = "cinepak_rs",
    .id                = AV_CODEC_ID_CINEPAK,
    .interesting       = interesting,
    .nb_interesting    = sizeof(interesting),
    .palette_side_data = 1,
    .config            = config,
    .gen               = gen,
    .synth_kinds       = "v1-rgb v4-rgb skip-rgb mixed-rgb codebooks-rgb v1-pal v4-pal mixed-pal",
    .synth             = synth,
};
