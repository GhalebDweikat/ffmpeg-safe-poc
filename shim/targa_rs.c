/*
 * Truevision Targa image decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of targa.c as the decoder "targa_rs".
 *
 * This file only talks to the FFmpeg API. A Targa packet says how large its
 * picture is and what pixel format it has, so Rust is asked twice: once for
 * what the header says, so that the frame can be allocated, and once to
 * decode into the visible picture. This file never reads packet bytes or
 * writes pixels itself. The FFmpeg calls are made in the order the C
 * decoder makes them, because a caller can see their effects: the pixel
 * format and the dimensions change even when the packet is then refused.
 */

#include <stddef.h>
#include <stdint.h>

#include "libavutil/imgutils.h"
#include "libavutil/internal.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"

#define FFSAFE_PANIC (-100)

enum {
    TARGA_RS_HDR_DECODE      = 0,
    TARGA_RS_HDR_NO_DATA     = 1,
    TARGA_RS_HDR_IMAGE_SHORT = 2,
    TARGA_RS_HDR_SHORT       = -1,
    TARGA_RS_HDR_DEPTH       = -2,
    TARGA_RS_HDR_PALETTE     = -3,
};

typedef struct TargaRsInfo {
    int32_t format;
    int32_t width;
    int32_t height;
    int32_t colormap_ignored;
} TargaRsInfo;

int targa_rs_header(const uint8_t *pkt, size_t pkt_size, TargaRsInfo *info);
int targa_rs_decode(const uint8_t *pkt, size_t pkt_size,
                    uint8_t *frame, size_t frame_size, size_t stride,
                    uint8_t *pal);

static int targa_rs_decode_frame(AVCodecContext *avctx, AVFrame *p,
                                 int *got_frame, AVPacket *avpkt)
{
    static const enum AVPixelFormat formats[] = {
        AV_PIX_FMT_GRAY8, AV_PIX_FMT_PAL8, AV_PIX_FMT_RGB555LE,
        AV_PIX_FMT_BGR24, AV_PIX_FMT_BGRA,
    };
    static const char *const messages[] = {
        [1] = "Palette entry size is not supported\n",
        [2] = "Not enough data to read palette\n",
        [3] = "Not enough data available for image\n",
        [4] = "Ran ouf of data before end-of-image\n",
        [5] = "Packet went out of bounds\n",
    };
    TargaRsInfo info;
    size_t stride, frame_size;
    int status, row_len, ret;

    status = targa_rs_header(avpkt->data, avpkt->size, &info);
    if (status < TARGA_RS_HDR_PALETTE || status > TARGA_RS_HDR_IMAGE_SHORT)
        return AVERROR_BUG;

    if (info.colormap_ignored)
        av_log(avctx, AV_LOG_WARNING, "File without colormap has colormap information set.\n");

    if (status == TARGA_RS_HDR_SHORT) {
        av_log(avctx, AV_LOG_ERROR, "Not enough data to read header\n");
        return AVERROR_INVALIDDATA;
    }
    if (status == TARGA_RS_HDR_DEPTH) {
        av_log(avctx, AV_LOG_ERROR, "Bit depth is not supported\n");
        return AVERROR_INVALIDDATA;
    }

    if (info.format < 0 || info.format >= FF_ARRAY_ELEMS(formats))
        return AVERROR_BUG;
    avctx->pix_fmt = formats[info.format];

    if (status == TARGA_RS_HDR_PALETTE) {
        av_log(avctx, AV_LOG_ERROR, "Incorrect palette\n");
        return AVERROR_INVALIDDATA;
    }

    if ((ret = ff_set_dimensions(avctx, info.width, info.height)) < 0)
        return ret;

    if (status == TARGA_RS_HDR_NO_DATA)
        return avpkt->size;

    if (status == TARGA_RS_HDR_IMAGE_SHORT) {
        av_log(avctx, AV_LOG_ERROR, "Not enough data available for image\n");
        return AVERROR_INVALIDDATA;
    }

    if ((ret = ff_get_buffer(avctx, p, 0)) < 0)
        return ret;
    p->pict_type = AV_PICTURE_TYPE_I;

    /* Rust is given the visible picture and nothing else: height - 1 full
     * rows plus one visible row, and the palette of a pal8 frame. */
    row_len = av_image_get_linesize(avctx->pix_fmt, avctx->width, 0);
    if (row_len <= 0 || avctx->height <= 0 ||
        p->width < avctx->width || p->height < avctx->height ||
        p->linesize[0] < row_len)
        return AVERROR_BUG;
    stride     = p->linesize[0];
    frame_size = stride * (avctx->height - 1) + row_len;

    ret = targa_rs_decode(avpkt->data, avpkt->size,
                          p->data[0], frame_size, stride,
                          avctx->pix_fmt == AV_PIX_FMT_PAL8 ? p->data[1] : NULL);
    if (ret < 0 || ret >= FF_ARRAY_ELEMS(messages))
        return AVERROR_BUG;
    if (ret > 0) {
        av_log(avctx, AV_LOG_ERROR, "%s", messages[ret]);
        return AVERROR_INVALIDDATA;
    }

    *got_frame = 1;

    return avpkt->size;
}

const FFCodec ff_targa_rs_decoder = {
    .p.name         = "targa_rs",
    CODEC_LONG_NAME("Truevision Targa image, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_TARGA,
    .p.capabilities = AV_CODEC_CAP_DR1,
    FF_CODEC_DECODE_CB(targa_rs_decode_frame),
};
