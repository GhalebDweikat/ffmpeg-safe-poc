/*
 * v210 (uncompressed 4:2:2 10-bit) decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of v210dec.c as the decoder "v210_rs".
 *
 * This file only talks to the FFmpeg API: it allocates the frame, hands
 * the packet and the three visible planes to Rust as (pointer, length)
 * pairs, and maps the result to an FFmpeg return code. It never reads
 * packet bytes or writes samples itself.
 *
 * Unlike the C decoder it does not split a frame across slice threads.
 */

#include <stddef.h>
#include <stdint.h>

#include "libavutil/internal.h"
#include "libavutil/opt.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "thread.h"

typedef struct V210RsLayout {
    size_t stride;
    size_t offset;
    uint32_t small_padding;
} V210RsLayout;

int v210_rs_layout(const uint8_t *pkt, size_t pkt_size, int width, int height,
                   uint32_t codec_tag, int custom_stride, V210RsLayout *out);
int v210_rs_decode(const uint8_t *pkt, size_t pkt_size, size_t stride, size_t offset,
                   uint8_t *y, size_t y_len, size_t y_stride,
                   uint8_t *u, uint8_t *v, size_t c_len, size_t c_stride,
                   int width, int height);

typedef struct V210RsContext {
    AVClass *av_class;
    int custom_stride;
    int stride_warning_shown;
} V210RsContext;

static av_cold int v210_rs_decode_init(AVCodecContext *avctx)
{
    avctx->pix_fmt             = AV_PIX_FMT_YUV422P10;
    avctx->bits_per_raw_sample = 10;

    return 0;
}

static int v210_rs_decode_frame(AVCodecContext *avctx, AVFrame *pic,
                                int *got_frame, AVPacket *avpkt)
{
    V210RsContext *s = avctx->priv_data;
    V210RsLayout lay = { 0 };
    size_t y_stride, c_stride, cw;
    int ret;

    ret = v210_rs_layout(avpkt->data, avpkt->size, avctx->width, avctx->height,
                         avctx->codec_tag, s->custom_stride, &lay);
    switch (ret) {
    case 0:
        break;
    case 1:
        av_log(avctx, AV_LOG_ERROR, "Strideless v210 is not supported for size %dx%d\n",
               avctx->width, avctx->height);
        return AVERROR_INVALIDDATA;
    case 2:
        av_log(avctx, AV_LOG_ERROR, "packet too small\n");
        return AVERROR_INVALIDDATA;
    default:
        return AVERROR_BUG;
    }
    if (lay.small_padding) {
        if (!s->stride_warning_shown)
            av_log(avctx, AV_LOG_WARNING,
                   "Broken v210 with too small padding (%u byte) detected\n",
                   lay.small_padding);
        s->stride_warning_shown = 1;
    }

    if ((ret = ff_thread_get_buffer(avctx, pic, 0)) < 0)
        return ret;

    /* Rust is given the visible samples of each plane and nothing else:
     * height - 1 full rows plus one visible row, in 16-bit units. */
    cw = ((size_t)avctx->width + 1) / 2;
    if (avctx->width <= 0 || avctx->height <= 0 ||
        pic->linesize[0] < 2 * (ptrdiff_t)avctx->width || (pic->linesize[0] & 1) ||
        pic->linesize[1] < 2 * (ptrdiff_t)cw || (pic->linesize[1] & 1) ||
        pic->linesize[2] != pic->linesize[1])
        return AVERROR_BUG;
    y_stride = pic->linesize[0] / 2;
    c_stride = pic->linesize[1] / 2;

    ret = v210_rs_decode(avpkt->data, avpkt->size, lay.stride, lay.offset,
                         pic->data[0], y_stride * (avctx->height - 1) + avctx->width, y_stride,
                         pic->data[1], pic->data[2],
                         c_stride * (avctx->height - 1) + cw, c_stride,
                         avctx->width, avctx->height);
    if (ret < 0)
        return AVERROR_BUG;

    if (avctx->field_order > AV_FIELD_PROGRESSIVE) {
        /* we have interlaced material flagged in container */
        pic->flags |= AV_FRAME_FLAG_INTERLACED;
        if (avctx->field_order == AV_FIELD_TT || avctx->field_order == AV_FIELD_TB)
            pic->flags |= AV_FRAME_FLAG_TOP_FIELD_FIRST;
    }

    *got_frame = 1;

    return avpkt->size;
}

#define V210DEC_FLAGS AV_OPT_FLAG_DECODING_PARAM | AV_OPT_FLAG_VIDEO_PARAM
static const AVOption v210_rs_options[] = {
    {"custom_stride", "Custom V210 stride", offsetof(V210RsContext, custom_stride), AV_OPT_TYPE_INT,
     {.i64 = 0}, -1, INT_MAX, V210DEC_FLAGS},
    {NULL}
};

static const AVClass v210_rs_class = {
    .class_name = "V210 Decoder (Rust)",
    .item_name  = av_default_item_name,
    .option     = v210_rs_options,
    .version    = LIBAVUTIL_VERSION_INT,
};

const FFCodec ff_v210_rs_decoder = {
    .p.name         = "v210_rs",
    CODEC_LONG_NAME("Uncompressed 4:2:2 10-bit, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_V210,
    .priv_data_size = sizeof(V210RsContext),
    .init           = v210_rs_decode_init,
    FF_CODEC_DECODE_CB(v210_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1 | AV_CODEC_CAP_FRAME_THREADS,
    .p.priv_class   = &v210_rs_class,
};
