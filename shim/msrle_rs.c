/*
 * Microsoft RLE video decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of msrle.c as the decoder "msrle_rs".
 *
 * This file only talks to the FFmpeg API: it allocates the frame, lets
 * FFmpeg update the palette, hands the packet and the visible picture to
 * Rust as (pointer, length) pairs, and logs the result. It never reads
 * packet bytes or writes pixels itself.
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

int msrle_rs_decode(const uint8_t *pkt, size_t pkt_size,
                    uint8_t *frame, size_t frame_size, size_t stride,
                    int width, int height, int bits);
int msrle_rs_palette(const uint8_t *extradata, size_t size, uint32_t *pal);

typedef struct MsrleRsContext {
    AVFrame *frame;
    uint32_t pal[256];
} MsrleRsContext;

static av_cold int msrle_rs_decode_init(AVCodecContext *avctx)
{
    MsrleRsContext *s = avctx->priv_data;

    switch (avctx->bits_per_coded_sample) {
    case 1:
        avctx->pix_fmt = AV_PIX_FMT_MONOWHITE;
        break;
    case 4:
    case 8:
        avctx->pix_fmt = AV_PIX_FMT_PAL8;
        break;
    case 24:
        avctx->pix_fmt = AV_PIX_FMT_BGR24;
        break;
    default:
        av_log(avctx, AV_LOG_ERROR, "unsupported bits per sample\n");
        return AVERROR_INVALIDDATA;
    }

    s->frame = av_frame_alloc();
    if (!s->frame)
        return AVERROR(ENOMEM);

    if (msrle_rs_palette(avctx->extradata, avctx->extradata_size, s->pal) < 0)
        return AVERROR_BUG;

    return 0;
}

static int msrle_rs_decode_frame(AVCodecContext *avctx, AVFrame *rframe,
                                 int *got_frame, AVPacket *avpkt)
{
    static const char *const messages[] = {
        [3]  = "Next line is beyond picture bounds\n",
        [4]  = "Skip beyond picture bounds\n",
        [5]  = "bytestream overrun\n",
        [6]  = "MS RLE: bytestream overrun\n",
        [7]  = "MS RLE: frame/stream ptr just went out of bounds (copy)\n",
        [8]  = "MS RLE: frame ptr just went out of bounds (run)\n",
        [9]  = "MS RLE: ended frame decode with bytes left over\n",
        [10] = "Unknown depth\n",
    };
    MsrleRsContext *s = avctx->priv_data;
    size_t stride, frame_size;
    int row_len, ret;

    if (avpkt->size < 2) /* at least an end-of-picture code should be there */
        return AVERROR_INVALIDDATA;

    if ((ret = ff_reget_buffer(avctx, s->frame, 0)) < 0)
        return ret;

    if (avctx->bits_per_coded_sample > 1 && avctx->bits_per_coded_sample <= 8) {
        ff_copy_palette(s->pal, avpkt, avctx);
        /* make the palette available */
        memcpy(s->frame->data[1], s->pal, AVPALETTE_SIZE);
    }

    /* Rust is given the visible picture and nothing else: height - 1 full
     * rows plus one visible row. */
    row_len = av_image_get_linesize(avctx->pix_fmt, avctx->width, 0);
    if (row_len <= 0 || avctx->height <= 0 ||
        s->frame->width < avctx->width || s->frame->height < avctx->height ||
        s->frame->linesize[0] < row_len)
        return AVERROR_BUG;
    stride     = s->frame->linesize[0];
    frame_size = stride * (avctx->height - 1) + row_len;

    ret = msrle_rs_decode(avpkt->data, avpkt->size,
                          s->frame->data[0], frame_size, stride,
                          avctx->width, avctx->height,
                          avctx->bits_per_coded_sample);
    if (ret < 0)
        return AVERROR_BUG;
    /* As in the C decoder, a damaged packet still yields a picture. */
    if (ret == 2)
        av_log(avctx, AV_LOG_WARNING, "MS RLE warning: no end-of-picture code\n");
    else if (ret >= 3 && ret < FF_ARRAY_ELEMS(messages))
        av_log(avctx, AV_LOG_ERROR, "%s", messages[ret]);

    if ((ret = av_frame_ref(rframe, s->frame)) < 0)
        return ret;

    *got_frame = 1;

    /* report that the buffer was completely consumed */
    return avpkt->size;
}

static av_cold void msrle_rs_decode_flush(AVCodecContext *avctx)
{
    MsrleRsContext *s = avctx->priv_data;

    av_frame_unref(s->frame);
}

static av_cold int msrle_rs_decode_end(AVCodecContext *avctx)
{
    MsrleRsContext *s = avctx->priv_data;

    av_frame_free(&s->frame);

    return 0;
}

const FFCodec ff_msrle_rs_decoder = {
    .p.name         = "msrle_rs",
    CODEC_LONG_NAME("Microsoft RLE, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_MSRLE,
    .priv_data_size = sizeof(MsrleRsContext),
    .init           = msrle_rs_decode_init,
    .close          = msrle_rs_decode_end,
    FF_CODEC_DECODE_CB(msrle_rs_decode_frame),
    .flush          = msrle_rs_decode_flush,
    .p.capabilities = AV_CODEC_CAP_DR1,
};
