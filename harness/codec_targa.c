/* Truevision Targa: packet generators. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libavutil/buffer.h"
#include "libavutil/imgutils.h"

#include "common.h"

/*
 * The decoder takes the picture size and format from each packet and
 * ignores what it was opened with, so one fuzz case mixes sizes and
 * formats. FFmpeg hands a decoder the same frame buffers again as long as
 * size and format stay the same, and the Targa decoder does not always
 * write every byte of a frame (see avoid_uninit below, and the palette), so
 * what one packet leaves in a buffer shows in a later picture. A case is
 * therefore mostly a sequence of pictures of one size.
 */

static void config(Cfg *cfg)
{
}

/*
 * A second comparison, made here because this file may not change
 * harness/common.c. frame_diff() there does not reach the palette of a pal8
 * frame: it stops at av_pix_fmt_count_planes(), which is 1 for pal8. And no
 * frame is compared when a packet is refused, although the decoder may have
 * written part of the picture, and the palette, into a buffer that FFmpeg
 * hands out again for a later picture.
 *
 * In half of the fuzz cases both decoders therefore get their frames
 * through get_buffer() below, which keeps a reference to the last frame of
 * each decoder. When the C decoder asks for its next frame, both decoders
 * are done with the packet before, and what they left in their frames must
 * be equal: every visible sample and the palette, whether the packet was
 * accepted or not. The other half of the cases leave the allocation alone,
 * because holding a reference delays the reuse of a buffer by one frame.
 */
typedef struct Seen {
    AVBufferRef *buf[2];
    int format, width, height, linesize;
    uint64_t nb;                /* frames this decoder has asked for */
} Seen;

static Seen seen[2];            /* the C decoder, the Rust port */

static void seen_differ(const char *what)
{
    printf("MISMATCH what=\"%s, in the frames the decoders left behind "
           "(get_buffer in codec_targa.c)\" c=%dx%d rs=%dx%d\n", what,
           seen[0].width, seen[0].height, seen[1].width, seen[1].height);
    fflush(stdout);
    exit(2);
}

static void compare_seen(void)
{
    const Seen *a = &seen[0], *b = &seen[1];
    int row;

    if (a->nb != b->nb)
        seen_differ("number of frames allocated");
    if (!a->buf[0] || !b->buf[0])
        return;
    if (a->format != b->format || a->width != b->width || a->height != b->height)
        seen_differ("frame parameters");
    row = av_image_get_linesize(a->format, a->width, 0);
    for (int y = 0; y < a->height; y++)
        if (memcmp(a->buf[0]->data + (size_t)y * a->linesize,
                   b->buf[0]->data + (size_t)y * b->linesize, row))
            seen_differ("plane 0");
    if (!a->buf[1] != !b->buf[1] ||
        (a->buf[1] && memcmp(a->buf[1]->data, b->buf[1]->data, AVPALETTE_SIZE)))
        seen_differ("palette");
}

static int get_buffer(AVCodecContext *ctx, AVFrame *frame, int flags)
{
    Seen *s = &seen[!strcmp(ctx->codec->name, "targa_rs")];
    int ret = avcodec_default_get_buffer2(ctx, frame, flags);

    if (ret < 0)
        return ret;
    if (s == &seen[0])
        compare_seen();
    av_buffer_unref(&s->buf[0]);
    av_buffer_unref(&s->buf[1]);
    /* The default allocator gives each plane a buffer of its own, starting
     * at the plane's first byte. */
    if (frame->buf[0] && frame->data[0] == frame->buf[0]->data &&
        (!frame->buf[1] || frame->data[1] == frame->buf[1]->data)) {
        s->buf[0] = av_buffer_ref(frame->buf[0]);
        if (frame->buf[1])
            s->buf[1] = av_buffer_ref(frame->buf[1]);
    }
    s->format   = frame->format;
    s->width    = frame->width;
    s->height   = frame->height;
    s->linesize = frame->linesize[0];
    s->nb++;
    return ret;
}

/* Fuzzing only: bound the picture size, so that a damaged header cannot
 * make both decoders allocate a gigabyte. Sometimes bound it so low that
 * ordinary sizes are refused when the dimensions are set. */
static void options(Cfg *cfg)
{
    cfg->opt[0] = rnd(8) ? 1 << 19 : 1 << 12;
    cfg->opt[1] = rnd(2);
}

static void setup(AVCodecContext *ctx, const Cfg *cfg)
{
    if (!cfg->opt[0])
        return;
    if (cfg->opt[1])
        ctx->get_buffer2 = get_buffer;
    ctx->max_pixels = cfg->opt[0];
    /* The decoder does not need to be told a size (an image file reader
     * does not know one either). Without this, opening would be refused
     * whenever the size the case was set up with is over the bound. */
    if (ctx->width * ctx->height > cfg->opt[0])
        ctx->width = ctx->height = 0;
}

static void put16(Buf *b, int v)
{
    put(b, v & 0xff);
    put(b, v >> 8);
}

static void put_random(Buf *b, int n)
{
    while (n >= 8 && b->n + 8 <= MAX_PKT) {
        uint64_t v = rnd64();
        memcpy(b->d + b->n, &v, 8);
        b->n += 8;
        n    -= 8;
    }
    while (n-- > 0)
        put(b, rnd(256));
}

static int advance(int *y, int h, int interleave)
{
    *y += interleave;
    if (*y < h)
        return 1;
    *y = (*y + 1) & (interleave - 1);
    return *y && *y < h;
}

/*
 * The one input the fuzzer stays away from. When the data ends inside the
 * pixel value of a run packet, the C decoder repeats bytes of a stack array
 * it has not initialised (uint8_t tmp[4] in targa_decode_rle), so its
 * output is not a function of the packet. This follows the decoder through
 * the packet and, if it finds such a run, appends the missing bytes of the
 * pixel value (at most 3). Everything else is left alone, including a
 * packet that ends right after a complete pixel value.
 */
static void avoid_uninit(Buf *b)
{
    const uint8_t *d = b->d;
    int n = b->n, p = 0;
    int idlen, pal, compr, first, colors, csize, w, h, bpp, flags;
    int depth, interleave, x = 0, y = 0, more = 1, pal8;

    /* To see the C decoder's behaviour on that input, run the fuzzer with
     * TARGA_FUZZ_UNINIT=1 in the environment. */
    if (getenv("TARGA_FUZZ_UNINIT"))
        return;

#define BYTE() (n - p >= 1 ? d[p++] : (p = n, 0))
#define LE16() (n - p >= 2 ? (p += 2, d[p - 2] | d[p - 1] << 8) : (p = n, 0))
#define SKIP(k) (p += (k) < n - p ? (k) : n - p)
    idlen  = BYTE();
    pal    = BYTE();
    compr  = BYTE();
    first  = LE16();
    colors = LE16();
    csize  = BYTE();
    SKIP(4);
    w      = LE16();
    h      = LE16();
    bpp    = BYTE();
    flags  = BYTE();
    if (!pal)
        first = colors = csize = 0;
    if (n - p < idlen + 2 * colors)
        return;
    SKIP(idlen);
    if (bpp != 8 && bpp != 15 && bpp != 16 && bpp != 24 && bpp != 32)
        return;
    if (colors && colors + first > 256)
        return;
    if (!w || !h || !(compr & 8) || !(compr & ~8))
        return;
    pal8 = bpp == 8 && (compr & ~8) != 3;
    if (colors) {
        int entry = csize == 32 ? 4 : csize == 24 ? 3 : csize == 15 || csize == 16 ? 2 : 0;
        if (!entry || (pal8 && n - p < colors * entry))
            return;
        SKIP(colors * entry);
    }
    depth      = (bpp + 1) >> 3;
    interleave = flags & 0x40 ? 2 : flags & 0x80 ? 4 : 1;
    while (more) {
        int type, count;
        if (n - p <= 0)
            return;
        type  = d[p++];
        count = (type & 0x7f) + 1;
        if (type & 0x80) {
            if (n - p < depth) {
                if (n + depth > MAX_PKT) {
                    b->n = p - 1;       /* no room: drop the packet instead */
                    return;
                }
                put_random(b, depth - (n - p));
                return;
            }
            p += depth;
        }
        do {
            int k = count < w - x ? count : w - x;
            if (!(type & 0x80))
                SKIP(k * depth);
            count -= k;
            x     += k;
            if (x == w) {
                x    = 0;
                more = advance(&y, h, interleave);
            }
        } while (more && count > 0);
    }
#undef BYTE
#undef LE16
#undef SKIP
}

static void fixup(Buf *b, const Cfg *cfg)
{
    avoid_uninit(b);
}

static int entry_bytes(int csize)
{
    return csize == 32 ? 4 : csize == 24 ? 3 : 2;
}

static void gen(Buf *b, const Cfg *cfg)
{
    static const int depths[] = { 8, 8, 8, 15, 16, 24, 24, 32 };
    static const int csizes[] = { 15, 16, 24, 24, 32 };
    int w = cfg->w, h = cfg->h;
    int bpp = depths[rnd(8)], depth, type, flags, idlen;
    int has_pal = 0, first = 0, colors = 0, csize = 0, last_literal = 0;

    b->n = 0;
    if (rnd(60) == 0) {                 /* noise */
        put_random(b, 1 + rnd(64));
        avoid_uninit(b);
        return;
    }
    if (rnd(12) == 0) {                 /* not the size of the last picture */
        w = 1 + rnd(40);
        h = 1 + rnd(40);
    }
    if (rnd(60) == 0)
        bpp = rnd(40);
    depth = (bpp + 1) >> 3;

    type = bpp == 8 ? (rnd(2) ? 3 : 1) : 2;
    if (rnd(30) == 0)
        type = rnd(8);                  /* 0 is "no picture" */
    if (rnd(3))
        type |= 8;
    if (rnd(100) == 0)
        type = rnd(256);

    flags = rnd(16);
    if (rnd(2))
        flags |= 0x20;                  /* top to bottom */
    if (rnd(3) == 0)
        flags |= 0x10;                  /* right to left */
    if (rnd(4) == 0)
        flags |= 0x40 << rnd(2);        /* interleaved */
    if (rnd(16) == 0)
        flags |= 0xc0;

    idlen = rnd(6) ? 0 : rnd(30);

    if ((type & 7) == 1 || rnd(12) == 0) {
        has_pal = rnd(20) ? 1 : 1 + rnd(255);
        colors  = rnd(3) ? 256 : rnd(257);
        first   = rnd(4) ? 0 : rnd(257 - colors);
        if (rnd(20) == 0)
            first += 1 + rnd(300);      /* too many entries */
        csize = rnd(30) ? csizes[rnd(5)] : rnd(40);
    } else if (rnd(20) == 0) {          /* a description without a colour map */
        first  = rnd(300);
        colors = rnd(300);
        csize  = rnd(40);
    }

    put(b, idlen);
    put(b, has_pal);
    put(b, type);
    put16(b, first);
    put16(b, colors);
    put(b, csize);
    put16(b, rnd(65536));
    put16(b, rnd(65536));
    put16(b, w);
    put16(b, h);
    put(b, bpp);
    put(b, flags);
    put_random(b, idlen);
    if (has_pal) {
        int n = colors * entry_bytes(csize);
        if (rnd(40) == 0)
            n = rnd(n + 1);             /* cut short */
        put_random(b, n);
    }

    if (!(type & 8)) {
        int n = w * h * depth;
        if (rnd(20) == 0)
            n += rnd(9) - 4;
        put_random(b, n);
    } else {
        int total = w * h, px = 0;
        if (rnd(12) == 0)
            total += rnd(5) - 2;
        while (px < total && b->n < MAX_PKT - 1024) {
            int n = 1 + rnd(rnd(3) ? 128 : 8);
            /* Packets run on from one row into the next. Now and then the
             * last one is too long. */
            if (n > total - px && rnd(16))
                n = total - px;
            if (rnd(2)) {
                put(b, 0x80 | (n - 1));
                put_random(b, depth);
                last_literal = 0;
            } else {
                put(b, n - 1);
                put_random(b, n * depth);
                last_literal = n * depth;
            }
            px += n;
        }
        /* A last literal packet without all of its pixels: the picture is
         * complete, and shows what the frame buffer held before. */
        if (last_literal && rnd(12) == 0) {
            b->n -= 1 + rnd(last_literal);
            avoid_uninit(b);
            return;
        }
    }
    if (rnd(3) == 0) {                  /* the footer FFmpeg's encoder writes */
        static const char footer[26] = "\0\0\0\0\0\0\0\0TRUEVISION-XFILE.";
        for (int i = 0; i < 26; i++)
            put(b, footer[i]);
    }
    avoid_uninit(b);
}

/*
 * One picture that takes a single decoder path for every pixel. Kinds:
 *   rawN        uncompressed, N bits, top to bottom
 *   rawpal      uncompressed, 8 bits with a 256-entry colour map
 *   runN        run packets of 128 pixels    (-short: 3 pixels)
 *   litN        literal packets of 128 pixels (-short: 3 pixels)
 *   mix24       runs of 5 and literal packets of 7, in turn
 *   flipN       uncompressed, right to left
 * Suffixes: -bu bottom to top, -il two-way interleaved.
 */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    AVPacket *pkt;
    int w = cfg->w, h = cfg->h;
    int is_run = !strncmp(kind, "run", 3), is_lit = !strncmp(kind, "lit", 3);
    int is_mix = !strncmp(kind, "mix", 3), is_flip = !strncmp(kind, "flip", 4);
    int is_pal = !strncmp(kind, "rawpal", 6), is_raw = !strncmp(kind, "raw", 3);
    int bpp = is_pal ? 8 : atoi(kind + strcspn(kind, "0123456789"));
    int depth = (bpp + 1) >> 3;
    int piece = strstr(kind, "-short") ? 3 : 128;
    int rle = is_run || is_lit || is_mix;
    int flags = 0x20, turn = 0;

    if (bpp != 8 && bpp != 15 && bpp != 16 && bpp != 24 && bpp != 32)
        return NULL;
    if (!rle && !is_raw && !is_flip)
        return NULL;
    if (strstr(kind, "-bu"))
        flags &= ~0x20;
    if (strstr(kind, "-il"))
        flags |= 0x40;
    if (is_flip)
        flags |= 0x10;

    b.n = 0;
    put(&b, 0);
    put(&b, is_pal);
    put(&b, (is_pal ? 1 : bpp == 8 ? 3 : 2) | (rle ? 8 : 0));
    put16(&b, 0);
    put16(&b, is_pal ? 256 : 0);
    put(&b, is_pal ? 24 : 0);
    put16(&b, 0);
    put16(&b, 0);
    put16(&b, w);
    put16(&b, h);
    put(&b, bpp);
    put(&b, flags);
    if (is_pal)
        put_random(&b, 256 * 3);

    if (!rle) {
        put_random(&b, w * h * depth);
    } else {
        for (int y = 0; y < h; y++) {
            for (int x = 0; x < w;) {
                int want = is_mix ? (turn & 1 ? 7 : 5) : piece;
                int n = w - x < want ? w - x : want;
                if (is_run || (is_mix && !(turn & 1))) {
                    put(&b, 0x80 | (n - 1));
                    put_random(&b, depth);
                } else {
                    put(&b, n - 1);
                    put_random(&b, n * depth);
                }
                x += n;
                turn++;
            }
        }
    }
    if (b.n >= MAX_PKT)
        return NULL;
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = {
    0x00, 0x01, 0x02, 0x03, 0x08, 0x09, 0x0a, 0x0b, 0x0f, 0x10, 0x18, 0x20,
    0x40, 0x7f, 0x80, 0x81, 0xff,
};

const CodecOps ops_targa = {
    .name           = "targa",
    .c_name         = "targa",
    .rs_name        = "targa_rs",
    .id             = AV_CODEC_ID_TARGA,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .options        = options,
    .setup          = setup,
    .gen            = gen,
    .fixup          = fixup,
    .synth_kinds    = "raw8 raw16 raw24 raw32 rawpal raw24-bu raw24-il "
                      "run8 run16 run24 run32 run8-short run24-short run32-short "
                      "lit8 lit16 lit24 lit32 lit24-short mix24 "
                      "flip8 flip16 flip24 flip32",
    .synth          = synth,
};
