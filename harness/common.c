#include <string.h>

#include "libavutil/imgutils.h"
#include "libavutil/mem.h"
#include "libavutil/pixdesc.h"

#include "common.h"

static const CodecOps *const all_codecs[] = {
    &ops_rpza,
    &ops_msrle,
    &ops_cinepak,
    &ops_v210,
};

const CodecOps *find_codec(const char *name)
{
    for (size_t i = 0; i < sizeof(all_codecs) / sizeof(all_codecs[0]); i++)
        if (all_codecs[i] && !strcmp(all_codecs[i]->name, name))
            return all_codecs[i];
    return NULL;
}

uint64_t rng = 0x9E3779B97F4A7C15ULL;

uint64_t rnd64(void)
{
    rng ^= rng >> 12;
    rng ^= rng << 25;
    rng ^= rng >> 27;
    return rng * 0x2545F4914F6CDD1DULL;
}

uint32_t rnd(uint32_t n)
{
    return n ? (uint32_t)((rnd64() >> 33) % n) : 0;
}

void mutate(Buf *b, const CodecOps *ops)
{
    int k = 1 + rnd(3);

    while (k--) {
        int pos, len, i;
        if (b->n == 0) {
            b->d[0] = rnd(256);
            b->n    = 1;
        }
        pos = rnd(b->n);
        switch (rnd(8)) {
        case 0:
            b->d[pos] ^= 1 << rnd(8);
            break;
        case 1:
            b->d[pos] = rnd(256);
            break;
        case 2:
            b->d[pos] = ops->nb_interesting ? ops->interesting[rnd(ops->nb_interesting)]
                                            : rnd(2) * 0xff;
            break;
        case 3:                         /* truncate */
            b->n = 1 + pos;
            break;
        case 4:                         /* delete a range */
            len = 1 + rnd(8);
            if (pos + len > b->n)
                len = b->n - pos;
            if (len < b->n) {
                memmove(b->d + pos, b->d + pos + len, b->n - pos - len);
                b->n -= len;
            }
            break;
        case 5:                         /* insert random bytes */
            len = 1 + rnd(8);
            if (b->n + len <= MAX_PKT) {
                memmove(b->d + pos + len, b->d + pos, b->n - pos);
                for (i = 0; i < len; i++)
                    b->d[pos + i] = rnd(256);
                b->n += len;
            }
            break;
        case 6:                         /* append garbage */
            len = 1 + rnd(40);
            for (i = 0; i < len; i++)
                put(b, rnd(256));
            break;
        default:                        /* copy a chunk over another */
            len = 1 + rnd(16);
            i   = rnd(b->n);
            if (pos + len > b->n)
                len = b->n - pos;
            if (i + len > b->n)
                len = b->n - i;
            memmove(b->d + pos, b->d + i, len);
            break;
        }
    }
}

void cfg_to_context(const Cfg *cfg, AVCodecContext *ctx)
{
    ctx->width                 = cfg->w;
    ctx->height                = cfg->h;
    ctx->bits_per_coded_sample = cfg->bits;
    ctx->codec_tag             = cfg->tag;
    if (cfg->extradata_size) {
        ctx->extradata = av_mallocz(cfg->extradata_size + AV_INPUT_BUFFER_PADDING_SIZE);
        memcpy(ctx->extradata, cfg->extradata, cfg->extradata_size);
        ctx->extradata_size = cfg->extradata_size;
    }
}

/* Visible bytes per row and rows of plane p; 0 rows when there is none. */
static void plane_extent(const AVFrame *f, int p, int *row_bytes, int *rows)
{
    const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(f->format);
    int planes = av_pix_fmt_count_planes(f->format);

    *row_bytes = *rows = 0;
    if (!desc || p >= planes)
        return;
    if (desc->flags & AV_PIX_FMT_FLAG_PAL) {
        if (p == 1) {                   /* the palette */
            *row_bytes = AVPALETTE_SIZE;
            *rows      = 1;
            return;
        }
        if (p > 1)
            return;
    }
    *row_bytes = av_image_get_linesize(f->format, f->width, p);
    *rows      = p == 1 || p == 2 ? AV_CEIL_RSHIFT(f->height, desc->log2_chroma_h)
                                  : f->height;
}

const char *frame_diff(const AVFrame *a, const AVFrame *b)
{
    static const char *const plane_names[4] = { "plane 0", "plane 1", "plane 2", "plane 3" };

    if (a->format != b->format)
        return "format";
    if (a->width != b->width || a->height != b->height)
        return "size";
    if (a->flags != b->flags)
        return "flags";
    if (a->pict_type != b->pict_type)
        return "pict_type";
    for (int p = 0; p < 4; p++) {
        int row_bytes, rows;
        plane_extent(a, p, &row_bytes, &rows);
        for (int y = 0; y < rows; y++)
            if (memcmp(a->data[p] + y * a->linesize[p],
                       b->data[p] + y * b->linesize[p], row_bytes))
                return plane_names[p];
    }
    return NULL;
}

uint64_t frame_hash(const AVFrame *f, uint64_t h)
{
    for (int p = 0; p < 4; p++) {
        int row_bytes, rows;
        plane_extent(f, p, &row_bytes, &rows);
        for (int y = 0; y < rows; y++) {
            const uint8_t *row = f->data[p] + y * f->linesize[p];
            for (int x = 0; x < row_bytes; x++)
                h = (h ^ row[x]) * 0x100000001b3ULL;
        }
    }
    return h;
}
