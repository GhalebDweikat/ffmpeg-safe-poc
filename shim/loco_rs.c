/*
 * LOCO decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of loco.c as the decoder "loco_rs".
 *
 * This file only talks to the FFmpeg API: it lets Rust read the extradata,
 * sets the pixel format, allocates the frame, hands the packet and the
 * visible part of each plane to Rust as (pointer, length) pairs, and maps
 * the result to an FFmpeg return code. It never reads packet bytes or
 * writes samples itself.
 */

#include <stddef.h>
#include <stdint.h>

#include "libavutil/imgutils.h"
#include "libavutil/internal.h"
#include "libavutil/pixdesc.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"

typedef struct LocoRsInfo {
    int version;
    int mode;
    int lossy;
    int format;                 /* 0: 4:2:2, 1: 4:2:0, 2: GBR, 3: GBRA */
} LocoRsInfo;

#define LOCO_RS_TOO_SMALL (-1)

int loco_rs_init(const uint8_t *extradata, size_t size, LocoRsInfo *info);
int loco_rs_decode(int format, int lossy, int width, int height,
                   const uint8_t *pkt, size_t pkt_size,
                   uint8_t *const *planes, const size_t *plane_sizes,
                   const size_t *strides);

typedef struct LocoRsContext {
    int lossy;
    int format;
} LocoRsContext;

static av_cold int loco_rs_decode_init(AVCodecContext *avctx)
{
    static const enum AVPixelFormat pix_fmts[4] = {
        AV_PIX_FMT_YUV422P, AV_PIX_FMT_YUV420P, AV_PIX_FMT_GBRP, AV_PIX_FMT_GBRAP,
    };
    LocoRsContext *l = avctx->priv_data;
    LocoRsInfo info  = { 0 };
    int ret;

    ret = loco_rs_init(avctx->extradata,
                       avctx->extradata_size > 0 ? avctx->extradata_size : 0, &info);
    if (ret < 0)
        return AVERROR_BUG;
    if (ret == 1) {
        av_log(avctx, AV_LOG_ERROR, "Extradata size must be >= 12 instead of %i\n",
               avctx->extradata_size);
        return AVERROR_INVALIDDATA;
    }
    if (info.version != 1 && info.version != 2)
        avpriv_request_sample(avctx, "LOCO codec version %i", info.version);
    if (ret == 2) {
        av_log(avctx, AV_LOG_ERROR, "lossy %i is too large\n", info.lossy);
        return AVERROR_INVALIDDATA;
    }
    if (ret == 3) {
        av_log(avctx, AV_LOG_INFO, "Unknown colorspace, index = %i\n", info.mode);
        return AVERROR_INVALIDDATA;
    }
    if (ret != 0 || info.format < 0 || info.format > 3)
        return AVERROR_BUG;

    l->lossy       = info.lossy;
    l->format      = info.format;
    avctx->pix_fmt = pix_fmts[info.format];
    if (avctx->debug & FF_DEBUG_PICT_INFO)
        av_log(avctx, AV_LOG_INFO, "lossy:%i, version:%i, mode: %i\n",
               info.lossy, info.version, info.mode);

    return 0;
}

static int loco_rs_decode_frame(AVCodecContext *avctx, AVFrame *p,
                                int *got_frame, AVPacket *avpkt)
{
    LocoRsContext *l = avctx->priv_data;
    const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(avctx->pix_fmt);
    uint8_t *planes[4] = { NULL };
    size_t sizes[4]    = { 0 };
    size_t strides[4]  = { 0 };
    int nb_planes, ret;

    if ((ret = ff_get_buffer(avctx, p, 0)) < 0)
        return ret;

    /* Rust is given the visible picture and nothing else: of every plane,
     * all rows but the last in full, and the visible part of the last. */
    nb_planes = av_pix_fmt_count_planes(avctx->pix_fmt);
    if (!desc || nb_planes < 3 || nb_planes > 4 ||
        p->width < avctx->width || p->height < avctx->height)
        return AVERROR_BUG;
    for (int i = 0; i < nb_planes; i++) {
        int row_len = av_image_get_linesize(avctx->pix_fmt, avctx->width, i);
        int rows    = i == 1 || i == 2 ? AV_CEIL_RSHIFT(avctx->height, desc->log2_chroma_h)
                                       : avctx->height;
        if (row_len <= 0 || rows <= 0 || !p->data[i] || p->linesize[i] < row_len)
            return AVERROR_BUG;
        planes[i]  = p->data[i];
        strides[i] = p->linesize[i];
        sizes[i]   = strides[i] * (rows - 1) + row_len;
    }

    ret = loco_rs_decode(l->format, l->lossy, avctx->width, avctx->height,
                         avpkt->data, avpkt->size, planes, sizes, strides);
    if (ret == LOCO_RS_TOO_SMALL) {
        av_log(avctx, AV_LOG_ERROR, "Input data too small.\n");
        return AVERROR(EINVAL);
    }
    if (ret < 0)
        return AVERROR_BUG;

    *got_frame = 1;

    return ret;
}

const FFCodec ff_loco_rs_decoder = {
    .p.name         = "loco_rs",
    CODEC_LONG_NAME("LOCO, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_LOCO,
    .priv_data_size = sizeof(LocoRsContext),
    .init           = loco_rs_decode_init,
    FF_CODEC_DECODE_CB(loco_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1,
};
