/*
 * QuickTime RPZA video decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of rpza.c as the decoder "rpza_rs".
 *
 * This file only talks to the FFmpeg API: it allocates the frame, hands the
 * packet and the visible image to Rust as (pointer, length) pairs, and maps
 * the result to an FFmpeg return code. It never reads packet bytes or
 * writes pixels itself.
 */

#include <stddef.h>
#include <stdint.h>

#include "libavutil/internal.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"

#define RPZA_RS_OK                        0
#define RPZA_RS_PACKET_TOO_SMALL        (-1)
#define RPZA_RS_FOUR_COLOR_TRUNCATED    (-2)
#define RPZA_RS_SIXTEEN_COLOR_TRUNCATED (-3)
#define RPZA_RS_NO_BLOCKS_LEFT          (-4)
#define RPZA_RS_UNKNOWN_OPCODE          (-5)
#define RPZA_RS_BAD_FRAME               (-6)
#define RPZA_RS_PANIC                   (-100)

typedef struct RpzaRsHeader {
    uint32_t first_byte;
    uint32_t chunk_size;
    uint32_t payload_size;
} RpzaRsHeader;

int rpza_rs_header(const uint8_t *pkt, size_t pkt_size, int width, int height,
                   RpzaRsHeader *out);
int rpza_rs_decode(const uint8_t *pkt, size_t pkt_size,
                   uint8_t *frame, size_t frame_size, size_t stride,
                   int width, int height, uint32_t *opcode);

typedef struct RpzaRsContext {
    AVFrame *frame;
} RpzaRsContext;

static av_cold int rpza_rs_decode_init(AVCodecContext *avctx)
{
    RpzaRsContext *s = avctx->priv_data;

    avctx->pix_fmt = AV_PIX_FMT_RGB555;

    s->frame = av_frame_alloc();
    if (!s->frame)
        return AVERROR(ENOMEM);

    return 0;
}

static int rpza_rs_decode_frame(AVCodecContext *avctx, AVFrame *rframe,
                                int *got_frame, AVPacket *avpkt)
{
    RpzaRsContext *s = avctx->priv_data;
    RpzaRsHeader hdr = { 0 };
    uint32_t opcode = 0;
    size_t stride, frame_size;
    int ret;

    ret = rpza_rs_header(avpkt->data, avpkt->size, avctx->width, avctx->height,
                         &hdr);
    if (ret == RPZA_RS_PANIC)
        return AVERROR_BUG;

    if (hdr.first_byte != 0xe1)
        av_log(avctx, AV_LOG_ERROR,
               "First chunk byte is 0x%02x instead of 0xe1\n", hdr.first_byte);
    if (hdr.chunk_size != hdr.payload_size + 4)
        av_log(avctx, AV_LOG_WARNING,
               "MOV chunk size %u != encoded chunk size %u\n",
               hdr.chunk_size, hdr.payload_size + 4);

    if (ret != RPZA_RS_OK)
        return AVERROR_INVALIDDATA;

    if ((ret = ff_reget_buffer(avctx, s->frame, 0)) < 0)
        return ret;

    /* Rust is given the visible image and nothing else: height - 1 full
     * rows plus one row of width pixels. */
    if (avctx->width <= 0 || avctx->height <= 0 ||
        s->frame->width < avctx->width || s->frame->height < avctx->height ||
        s->frame->linesize[0] < 2 * (ptrdiff_t)avctx->width)
        return AVERROR_BUG;
    stride     = s->frame->linesize[0];
    frame_size = stride * (avctx->height - 1) + 2 * (size_t)avctx->width;

    ret = rpza_rs_decode(avpkt->data, avpkt->size,
                         s->frame->data[0], frame_size, stride,
                         avctx->width, avctx->height, &opcode);
    switch (ret) {
    case RPZA_RS_OK:
        break;
    case RPZA_RS_NO_BLOCKS_LEFT:
        av_log(avctx, AV_LOG_ERROR,
               "Block counter just went negative (this should not happen)\n");
        return AVERROR_INVALIDDATA;
    case RPZA_RS_UNKNOWN_OPCODE:
        av_log(avctx, AV_LOG_ERROR, "Unknown opcode %u in rpza chunk.\n",
               opcode);
        return AVERROR_INVALIDDATA;
    case RPZA_RS_PACKET_TOO_SMALL:
    case RPZA_RS_FOUR_COLOR_TRUNCATED:
    case RPZA_RS_SIXTEEN_COLOR_TRUNCATED:
        return AVERROR_INVALIDDATA;
    default:
        return AVERROR_BUG;
    }

    if ((ret = av_frame_ref(rframe, s->frame)) < 0)
        return ret;

    *got_frame = 1;

    /* always report that the buffer was completely consumed */
    return avpkt->size;
}

static av_cold int rpza_rs_decode_end(AVCodecContext *avctx)
{
    RpzaRsContext *s = avctx->priv_data;

    av_frame_free(&s->frame);

    return 0;
}

const FFCodec ff_rpza_rs_decoder = {
    .p.name         = "rpza_rs",
    CODEC_LONG_NAME("QuickTime video (RPZA), safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_RPZA,
    .priv_data_size = sizeof(RpzaRsContext),
    .init           = rpza_rs_decode_init,
    .close          = rpza_rs_decode_end,
    FF_CODEC_DECODE_CB(rpza_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1,
};
