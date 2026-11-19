/*
 * Microsoft Video 1 decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of msvideo1.c as the decoder "msvideo1_rs".
 *
 * This file only talks to the FFmpeg API: it keeps the previous frame,
 * lets FFmpeg update the palette, hands the packet and the visible picture
 * to Rust as (pointer, length) pairs, and maps the result to an FFmpeg
 * return code. It never reads packet bytes or writes pixels itself.
 */

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "libavutil/internal.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"

int msvideo1_rs_check(size_t pkt_size, int width, int height);
int msvideo1_rs_decode(const uint8_t *pkt, size_t pkt_size,
                       uint8_t *frame, size_t frame_size, size_t stride,
                       int width, int height, int palette);

typedef struct Msvideo1RsContext {
    AVFrame *frame;
    int mode_8bit;  /* if it's not 8-bit, it's 16-bit */
    uint32_t pal[256];
} Msvideo1RsContext;

static av_cold int msvideo1_rs_decode_init(AVCodecContext *avctx)
{
    Msvideo1RsContext *s = avctx->priv_data;

    if (avctx->width < 4 || avctx->height < 4)
        return AVERROR_INVALIDDATA;

    /* figure out the colorspace based on the presence of a palette */
    if (avctx->bits_per_coded_sample == 8) {
        s->mode_8bit = 1;
        avctx->pix_fmt = AV_PIX_FMT_PAL8;
        if (avctx->extradata_size >= AVPALETTE_SIZE)
            memcpy(s->pal, avctx->extradata, AVPALETTE_SIZE);
    } else {
        s->mode_8bit = 0;
        avctx->pix_fmt = AV_PIX_FMT_RGB555;
    }

    s->frame = av_frame_alloc();
    if (!s->frame)
        return AVERROR(ENOMEM);

    return 0;
}

static int msvideo1_rs_decode_frame(AVCodecContext *avctx, AVFrame *rframe,
                                    int *got_frame, AVPacket *avpkt)
{
    Msvideo1RsContext *s = avctx->priv_data;
    size_t stride, row_len, frame_size;
    int ret;

    /* Discard frame if its smaller than the minimum frame size */
    ret = msvideo1_rs_check(avpkt->size, avctx->width, avctx->height);
    if (ret < 0)
        return AVERROR_BUG;
    if (!ret) {
        av_log(avctx, AV_LOG_ERROR, "Packet is too small\n");
        return AVERROR_INVALIDDATA;
    }

    if ((ret = ff_reget_buffer(avctx, s->frame, 0)) < 0)
        return ret;

    if (s->mode_8bit)
        ff_copy_palette(s->pal, avpkt, avctx);

    /* Rust is given the visible picture and nothing else: height - 1 full
     * rows plus one visible row. */
    row_len = (size_t)avctx->width * (s->mode_8bit ? 1 : 2);
    if (avctx->width <= 0 || avctx->height <= 0 ||
        s->frame->width < avctx->width || s->frame->height < avctx->height ||
        s->frame->linesize[0] < (ptrdiff_t)row_len)
        return AVERROR_BUG;
    stride     = s->frame->linesize[0];
    frame_size = stride * (avctx->height - 1) + row_len;

    ret = msvideo1_rs_decode(avpkt->data, avpkt->size,
                             s->frame->data[0], frame_size, stride,
                             avctx->width, avctx->height, s->mode_8bit);
    if (ret < 0)
        return AVERROR_BUG;
    if (ret) {
        /* As in the C decoder, a packet that runs out still yields a
         * picture, but one that does not get the current palette. */
        av_log(avctx, AV_LOG_ERROR, " MS Video-1 warning: stream_ptr out of bounds\n");
    } else if (s->mode_8bit && avctx->pix_fmt == AV_PIX_FMT_PAL8) {
        /* make the palette available on the way out */
        memcpy(s->frame->data[1], s->pal, AVPALETTE_SIZE);
    }

    if ((ret = av_frame_ref(rframe, s->frame)) < 0)
        return ret;

    *got_frame = 1;

    /* report that the buffer was completely consumed */
    return avpkt->size;
}

static av_cold int msvideo1_rs_decode_end(AVCodecContext *avctx)
{
    Msvideo1RsContext *s = avctx->priv_data;

    av_frame_free(&s->frame);

    return 0;
}

const FFCodec ff_msvideo1_rs_decoder = {
    .p.name         = "msvideo1_rs",
    CODEC_LONG_NAME("Microsoft Video 1, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_MSVIDEO1,
    .priv_data_size = sizeof(Msvideo1RsContext),
    .init           = msvideo1_rs_decode_init,
    .close          = msvideo1_rs_decode_end,
    FF_CODEC_DECODE_CB(msvideo1_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1,
};
