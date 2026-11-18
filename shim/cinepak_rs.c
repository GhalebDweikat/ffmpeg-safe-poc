/*
 * Cinepak video decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of cinepak.c as the decoder "cinepak_rs".
 *
 * This file only talks to the FFmpeg API: it owns the Rust decoder object,
 * allocates the frame, lets FFmpeg update the palette, hands the packet and
 * the visible picture to Rust as (pointer, length) pairs, and maps the
 * result to an FFmpeg return code. It never reads packet bytes or writes
 * pixels itself.
 */

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "libavutil/internal.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"

typedef struct CinepakRs CinepakRs;

CinepakRs *cinepak_rs_new(int width, int height, int bits);
void cinepak_rs_free(CinepakRs *dec);
int cinepak_rs_check(CinepakRs *dec, const uint8_t *pkt, size_t pkt_size,
                     int discard_damaged_percentage, int has_palette);
int cinepak_rs_decode(CinepakRs *dec, const uint8_t *pkt, size_t pkt_size,
                      uint8_t *frame, size_t frame_size, size_t stride,
                      int width, int height, int *key);

typedef struct CinepakRsContext {
    CinepakRs *dec;
    AVFrame *frame;
    int palette_video;
    uint32_t pal[256];
} CinepakRsContext;

static av_cold int cinepak_rs_decode_init(AVCodecContext *avctx)
{
    CinepakRsContext *s = avctx->priv_data;

    /* check for paletted data */
    s->palette_video = avctx->bits_per_coded_sample == 8;
    avctx->pix_fmt   = s->palette_video ? AV_PIX_FMT_PAL8 : AV_PIX_FMT_RGB24;

    s->frame = av_frame_alloc();
    if (!s->frame)
        return AVERROR(ENOMEM);

    s->dec = cinepak_rs_new(avctx->width, avctx->height,
                            avctx->bits_per_coded_sample);
    if (!s->dec)
        return AVERROR(ENOMEM);

    return 0;
}

static int cinepak_rs_decode_frame(AVCodecContext *avctx, AVFrame *rframe,
                                   int *got_frame, AVPacket *avpkt)
{
    CinepakRsContext *s = avctx->priv_data;
    size_t stride, row_len, frame_size;
    int ret, key = 0;

    ret = cinepak_rs_check(s->dec, avpkt->data, avpkt->size,
                           avctx->discard_damaged_percentage,
                           !!av_packet_get_side_data(avpkt, AV_PKT_DATA_PALETTE, NULL));
    switch (ret) {
    case 0:
        break;
    case 1:
        return AVERROR_INVALIDDATA;
    case 2:                     /* empty frame, do not waste time */
        return avpkt->size;
    case 3:
        av_log(avctx, AV_LOG_ERROR, "cinepak_predecode_check failed\n");
        return AVERROR_INVALIDDATA;
    case 4:
        avpriv_request_sample(avctx, "encoded_buf_size 0");
        av_log(avctx, AV_LOG_ERROR, "cinepak_predecode_check failed\n");
        return AVERROR_PATCHWELCOME;
    default:
        return AVERROR_BUG;
    }

    if ((ret = ff_reget_buffer(avctx, s->frame, 0)) < 0)
        return ret;

    if (s->palette_video)
        ff_copy_palette(s->pal, avpkt, avctx);

    /* Rust is given the visible picture and nothing else: height - 1 full
     * rows plus one visible row. */
    row_len = (size_t)avctx->width * (s->palette_video ? 1 : 3);
    if (avctx->width <= 0 || avctx->height <= 0 ||
        s->frame->width < avctx->width || s->frame->height < avctx->height ||
        s->frame->linesize[0] < (ptrdiff_t)row_len)
        return AVERROR_BUG;
    stride     = s->frame->linesize[0];
    frame_size = stride * (avctx->height - 1) + row_len;

    ret = cinepak_rs_decode(s->dec, avpkt->data, avpkt->size,
                            s->frame->data[0], frame_size, stride,
                            avctx->width, avctx->height, &key);
    if (ret < 0)
        return AVERROR_BUG;
    if (key)
        s->frame->flags |= AV_FRAME_FLAG_KEY;
    else
        s->frame->flags &= ~AV_FRAME_FLAG_KEY;
    /* As in the C decoder, a damaged packet still yields a picture. */
    if (ret)
        av_log(avctx, AV_LOG_ERROR, "cinepak_decode failed\n");

    if (s->palette_video)
        memcpy(s->frame->data[1], s->pal, AVPALETTE_SIZE);

    if ((ret = av_frame_ref(rframe, s->frame)) < 0)
        return ret;

    *got_frame = 1;

    /* report that the buffer was completely consumed */
    return avpkt->size;
}

static av_cold int cinepak_rs_decode_end(AVCodecContext *avctx)
{
    CinepakRsContext *s = avctx->priv_data;

    av_frame_free(&s->frame);
    cinepak_rs_free(s->dec);
    s->dec = NULL;

    return 0;
}

const FFCodec ff_cinepak_rs_decoder = {
    .p.name         = "cinepak_rs",
    CODEC_LONG_NAME("Cinepak, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_CINEPAK,
    .priv_data_size = sizeof(CinepakRsContext),
    .init           = cinepak_rs_decode_init,
    .close          = cinepak_rs_decode_end,
    FF_CODEC_DECODE_CB(cinepak_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1,
    .caps_internal  = FF_CODEC_CAP_INIT_CLEANUP,
};
