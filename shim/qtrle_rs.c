/*
 * QuickTime Animation (RLE) video decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of qtrle.c as the decoder "qtrle_rs".
 *
 * This file only talks to the FFmpeg API: it asks Rust what the packet
 * header says, keeps the previous frame, lets FFmpeg update the palette,
 * hands the packet and the visible picture to Rust as (pointer, length)
 * pairs, and logs the result. It never reads packet bytes or writes pixels
 * itself.
 */

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "libavutil/imgutils.h"
#include "libavutil/internal.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"

#define FFSAFE_PANIC (-100)

int qtrle_rs_check(const uint8_t *pkt, size_t pkt_size, int height,
                   int discard_damaged_percentage);
int qtrle_rs_decode(const uint8_t *pkt, size_t pkt_size,
                    uint8_t *frame, size_t frame_size, size_t stride,
                    int width, int height, int bits);

typedef struct QtrleRsContext {
    AVFrame *frame;
    uint32_t pal[256];
} QtrleRsContext;

static av_cold int qtrle_rs_decode_init(AVCodecContext *avctx)
{
    QtrleRsContext *s = avctx->priv_data;

    switch (avctx->bits_per_coded_sample) {
    case 1:
    case 2:
    case 4:
    case 8:
    case 33:
    case 34:
    case 36:
    case 40:
        avctx->pix_fmt = AV_PIX_FMT_PAL8;
        break;
    case 16:
        avctx->pix_fmt = AV_PIX_FMT_RGB555;
        break;
    case 24:
        avctx->pix_fmt = AV_PIX_FMT_RGB24;
        break;
    case 32:
        avctx->pix_fmt = AV_PIX_FMT_ARGB;
        break;
    default:
        av_log(avctx, AV_LOG_ERROR, "Unsupported colorspace: %d bits/sample?\n",
               avctx->bits_per_coded_sample);
        return AVERROR_INVALIDDATA;
    }

    s->frame = av_frame_alloc();
    if (!s->frame)
        return AVERROR(ENOMEM);

    return 0;
}

static int qtrle_rs_decode_frame(AVCodecContext *avctx, AVFrame *rframe,
                                 int *got_frame, AVPacket *avpkt)
{
    QtrleRsContext *s = avctx->priv_data;
    size_t stride, frame_size;
    int row_len, ret;

    ret = qtrle_rs_check(avpkt->data, avpkt->size, avctx->height,
                         avctx->discard_damaged_percentage);
    if (ret == 2)
        return AVERROR_INVALIDDATA;
    if (ret < 0)
        return AVERROR_BUG;

    if (ret == 1) {
        /* The picture does not change. ff_reget_buffer() is not needed
         * then, so just update the frame properties. */
        if (!s->frame->data[0])
            return AVERROR_INVALIDDATA;
        ret = ff_decode_frame_props(avctx, s->frame);
        if (ret < 0)
            return ret;
    } else {
        if ((ret = ff_reget_buffer(avctx, s->frame, 0)) < 0)
            return ret;

        /* Rust is given the visible picture and nothing else: height - 1
         * full rows plus one visible row. */
        row_len = av_image_get_linesize(avctx->pix_fmt, avctx->width, 0);
        if (row_len <= 0 || avctx->height <= 0 ||
            s->frame->width < avctx->width || s->frame->height < avctx->height ||
            s->frame->linesize[0] < row_len)
            return AVERROR_BUG;
        stride     = s->frame->linesize[0];
        frame_size = stride * (avctx->height - 1) + row_len;

        ret = qtrle_rs_decode(avpkt->data, avpkt->size,
                              s->frame->data[0], frame_size, stride,
                              avctx->width, avctx->height,
                              avctx->bits_per_coded_sample);
        if (ret < 0 || ret > 1)
            return AVERROR_BUG;
        /* As in the C decoder, a damaged packet still yields a picture. */
        if (ret == 1)
            av_log(avctx, AV_LOG_ERROR, "Problem: pixel_ptr out of bounds\n");

        if (avctx->pix_fmt == AV_PIX_FMT_PAL8) {
            ff_copy_palette(s->pal, avpkt, avctx);
            /* make the palette available on the way out */
            memcpy(s->frame->data[1], s->pal, AVPALETTE_SIZE);
        }
    }

    if ((ret = av_frame_ref(rframe, s->frame)) < 0)
        return ret;
    *got_frame = 1;

    /* always report that the buffer was completely consumed */
    return avpkt->size;
}

static av_cold void qtrle_rs_decode_flush(AVCodecContext *avctx)
{
    QtrleRsContext *s = avctx->priv_data;

    av_frame_unref(s->frame);
}

static av_cold int qtrle_rs_decode_end(AVCodecContext *avctx)
{
    QtrleRsContext *s = avctx->priv_data;

    av_frame_free(&s->frame);

    return 0;
}

const FFCodec ff_qtrle_rs_decoder = {
    .p.name         = "qtrle_rs",
    CODEC_LONG_NAME("QuickTime Animation (RLE) video, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_QTRLE,
    .priv_data_size = sizeof(QtrleRsContext),
    .init           = qtrle_rs_decode_init,
    .close          = qtrle_rs_decode_end,
    FF_CODEC_DECODE_CB(qtrle_rs_decode_frame),
    .flush          = qtrle_rs_decode_flush,
    .p.capabilities = AV_CODEC_CAP_DR1,
};
