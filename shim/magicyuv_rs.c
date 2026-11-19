/*
 * MagicYUV decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of magicyuv.c as the decoder "magicyuv_rs".
 *
 * This file only talks to the FFmpeg API: it owns the Rust decoder object,
 * sets the pixel format and picture size Rust read from the header,
 * allocates the frame, hands the packet and the visible planes to Rust as
 * (pointer, length) pairs, and maps the results to FFmpeg return codes
 * and frame properties. It never reads packet bytes or writes samples
 * itself.
 *
 * Unlike the C decoder it does not run the slices of a frame on several
 * threads.
 */

#include <stddef.h>
#include <stdint.h>

#include "libavutil/common.h"
#include "libavutil/internal.h"
#include "libavutil/pixdesc.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"
#include "thread.h"

typedef struct MagicyuvRs MagicyuvRs;

typedef struct MagicyuvRsHeader {
    int32_t format;
    int32_t color_matrix;
    int32_t flags;
    int32_t width;
    int32_t height;
    int64_t detail;
} MagicyuvRsHeader;

MagicyuvRs *magicyuv_rs_new(void);
void magicyuv_rs_free(MagicyuvRs *dec);
int magicyuv_rs_header(MagicyuvRs *dec, const uint8_t *pkt, size_t pkt_size,
                       MagicyuvRsHeader *out);
int magicyuv_rs_tables(MagicyuvRs *dec, const uint8_t *pkt, size_t pkt_size,
                       int coded_width, int coded_height, int64_t *detail);
int magicyuv_rs_decode(MagicyuvRs *dec, const uint8_t *pkt, size_t pkt_size,
                       uint8_t *const *data, const size_t *len,
                       const size_t *linesize, int nb_planes);

typedef struct MagicyuvRsContext {
    MagicyuvRs *dec;
} MagicyuvRsContext;

/* The return code for an error number from Rust, with the C decoder's log
 * message. */
static int magicyuv_rs_error(AVCodecContext *avctx, int err, int64_t detail)
{
    switch (err) {
    case 1:
        return AVERROR_INVALIDDATA;
    case 2:
        av_log(avctx, AV_LOG_ERROR, "header or packet too small %"PRIu32"\n",
               (uint32_t)detail);
        return AVERROR_INVALIDDATA;
    case 3:
        avpriv_request_sample(avctx, "Version %d", (int)detail);
        return AVERROR_PATCHWELCOME;
    case 4:
        avpriv_request_sample(avctx, "Format 0x%X", (int)detail);
        return AVERROR_PATCHWELCOME;
    case 5:
        avpriv_request_sample(avctx, "Slice width %"PRIu32, (uint32_t)detail);
        return AVERROR_PATCHWELCOME;
    case 6:
        av_log(avctx, AV_LOG_ERROR, "invalid slice height: %d\n", (int)detail);
        return AVERROR_INVALIDDATA;
    case 7:
        av_log(avctx, AV_LOG_ERROR,
               "slice_height %d is not aligned to chroma vertical subsampling\n",
               (int)detail);
        return AVERROR_INVALIDDATA;
    case 8:
        av_log(avctx, AV_LOG_ERROR, "invalid number of slices: %d\n", (int)detail);
        return AVERROR_INVALIDDATA;
    case 9:
        av_log(avctx, AV_LOG_ERROR, "impossible slice height\n");
        return AVERROR_INVALIDDATA;
    case 10:
        av_log(avctx, AV_LOG_ERROR, "impossible height\n");
        return AVERROR_INVALIDDATA;
    case 11:
        av_log(avctx, AV_LOG_ERROR, "Invalid Huffman codes\n");
        return AVERROR_INVALIDDATA;
    case 12:
        av_log(avctx, AV_LOG_ERROR, "Cannot build Huffman codes\n");
        return AVERROR_INVALIDDATA;
    case 13:
        av_log(avctx, AV_LOG_ERROR, "Huffman tables too short\n");
        return AVERROR_INVALIDDATA;
    default:
        return AVERROR_BUG;
    }
}

static enum AVPixelFormat magicyuv_rs_pix_fmt(int format)
{
    switch (format) {
    case 0x65: return AV_PIX_FMT_GBRP;
    case 0x66: return AV_PIX_FMT_GBRAP;
    case 0x67: return AV_PIX_FMT_YUV444P;
    case 0x68: return AV_PIX_FMT_YUV422P;
    case 0x69: return AV_PIX_FMT_YUV420P;
    case 0x6a: return AV_PIX_FMT_YUVA444P;
    case 0x6b: return AV_PIX_FMT_GRAY8;
    case 0x6c: return AV_PIX_FMT_YUV422P10;
    case 0x76: return AV_PIX_FMT_YUV444P10;
    case 0x6d: return AV_PIX_FMT_GBRP10;
    case 0x6e: return AV_PIX_FMT_GBRAP10;
    case 0x6f: return AV_PIX_FMT_GBRP12;
    case 0x70: return AV_PIX_FMT_GBRAP12;
    case 0x71: return AV_PIX_FMT_GBRP14;
    case 0x72: return AV_PIX_FMT_GBRAP14;
    case 0x73: return AV_PIX_FMT_GRAY10;
    case 0x7b: return AV_PIX_FMT_YUV420P10;
    default:   return AV_PIX_FMT_NONE;
    }
}

static int magicyuv_rs_decode_frame(AVCodecContext *avctx, AVFrame *p,
                                    int *got_frame, AVPacket *avpkt)
{
    MagicyuvRsContext *s = avctx->priv_data;
    MagicyuvRsHeader hdr;
    const AVPixFmtDescriptor *desc;
    uint8_t *data[4];
    size_t len[4], linesize[4];
    int64_t detail = 0;
    int ret, planes, bytes, is_rgb;

    ret = magicyuv_rs_header(s->dec, avpkt->data, avpkt->size, &hdr);
    if (ret)
        return magicyuv_rs_error(avctx, ret, hdr.detail);

    avctx->pix_fmt = magicyuv_rs_pix_fmt(hdr.format);
    desc = av_pix_fmt_desc_get(avctx->pix_fmt);
    if (!desc)
        return AVERROR_BUG;
    is_rgb = !!(desc->flags & AV_PIX_FMT_FLAG_RGB);
    planes = av_pix_fmt_count_planes(avctx->pix_fmt);
    bytes  = desc->comp[0].depth > 8 ? 2 : 1;

    ret = ff_set_dimensions(avctx, hdr.width, hdr.height);
    if (ret < 0)
        return ret;

    ret = magicyuv_rs_tables(s->dec, avpkt->data, avpkt->size,
                             avctx->coded_width, avctx->coded_height, &detail);
    if (ret)
        return magicyuv_rs_error(avctx, ret, detail);

    if ((ret = ff_thread_get_buffer(avctx, p, 0)) < 0)
        return ret;

    /* Rust is given the visible samples of each plane and nothing else:
     * rows - 1 full rows plus one visible row. */
    if (planes < 1 || planes > 4)
        return AVERROR_BUG;
    for (int i = 0; i < planes; i++) {
        /* Signed on purpose: AV_CEIL_RSHIFT negates its argument, so on an
         * unsigned value it gives a number near 2^63 and the length below
         * would cover most of the address space. */
        int w = avctx->coded_width, h = avctx->coded_height;

        if (i == 1 || i == 2) {
            w = AV_CEIL_RSHIFT(w, desc->log2_chroma_w);
            h = AV_CEIL_RSHIFT(h, desc->log2_chroma_h);
        }
        if (!p->data[i] || w <= 0 || h <= 0 || p->linesize[i] < w * bytes ||
            p->width < avctx->coded_width || p->height < avctx->coded_height)
            return AVERROR_BUG;
        data[i]     = p->data[i];
        linesize[i] = p->linesize[i];
        len[i]      = linesize[i] * (size_t)(h - 1) + (size_t)w * bytes;
    }

    ret = magicyuv_rs_decode(s->dec, avpkt->data, avpkt->size,
                             data, len, linesize, planes);
    if (ret < 0)
        return AVERROR_BUG;
    /* As in the C decoder, a damaged slice still yields a picture. */
    if (ret)
        avpriv_request_sample(avctx, "Unknown prediction");

    if (is_rgb) {
        FFSWAP(uint8_t*, p->data[0], p->data[1]);
        FFSWAP(int, p->linesize[0], p->linesize[1]);
    } else {
        switch (hdr.color_matrix) {
        case 1:
            p->colorspace = AVCOL_SPC_BT470BG;
            break;
        case 2:
            p->colorspace = AVCOL_SPC_BT709;
            break;
        }
        p->color_range = (hdr.flags & 4) ? AVCOL_RANGE_JPEG : AVCOL_RANGE_MPEG;
    }

    *got_frame = 1;

    return avpkt->size;
}

static av_cold int magicyuv_rs_decode_init(AVCodecContext *avctx)
{
    MagicyuvRsContext *s = avctx->priv_data;

    s->dec = magicyuv_rs_new();
    if (!s->dec)
        return AVERROR(ENOMEM);

    return 0;
}

static av_cold int magicyuv_rs_decode_end(AVCodecContext *avctx)
{
    MagicyuvRsContext *s = avctx->priv_data;

    magicyuv_rs_free(s->dec);
    s->dec = NULL;

    return 0;
}

const FFCodec ff_magicyuv_rs_decoder = {
    .p.name         = "magicyuv_rs",
    CODEC_LONG_NAME("MagicYUV video, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_MAGICYUV,
    .priv_data_size = sizeof(MagicyuvRsContext),
    .init           = magicyuv_rs_decode_init,
    .close          = magicyuv_rs_decode_end,
    FF_CODEC_DECODE_CB(magicyuv_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1 | AV_CODEC_CAP_FRAME_THREADS,
    .caps_internal  = FF_CODEC_CAP_INIT_CLEANUP,
};
