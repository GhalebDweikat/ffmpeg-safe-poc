/*
 * TTA (True Audio) decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of tta.c as the decoder "tta_rs".
 *
 * This file only talks to the FFmpeg API: it owns the Rust decoder object,
 * applies the stream parameters Rust read from the extradata to the codec
 * context, allocates the frame, hands the packet and the frame's sample
 * buffer to Rust as (pointer, length) pairs, and maps the result to an
 * FFmpeg return code. It never reads packet bytes or writes samples
 * itself.
 */

#include <limits.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "libavutil/channel_layout.h"
#include "libavutil/opt.h"
#include "avcodec.h"
#include "codec_internal.h"
#include "thread.h"

typedef struct TtaRs TtaRs;

typedef struct TtaRsInfo {
    int channels;
    int bits_per_raw_sample;
    int sample_rate;
    int bps;
    int frame_length;
} TtaRsInfo;

int tta_rs_new(const uint8_t *extradata, size_t extradata_size,
               const uint8_t *password, size_t password_len,
               TtaRsInfo *info, TtaRs **dec);
void tta_rs_free(TtaRs *dec);
int tta_rs_crc_ok(const uint8_t *pkt, size_t pkt_size);
int tta_rs_decode(TtaRs *dec, const uint8_t *pkt, size_t pkt_size,
                  uint8_t *out, size_t out_bytes);

typedef struct TtaRsContext {
    AVClass *class;
    TtaRs *dec;
    TtaRsInfo info;
    char *pass;
} TtaRsContext;

static const int64_t tta_rs_channel_layouts[7] = {
    AV_CH_LAYOUT_STEREO,
    AV_CH_LAYOUT_STEREO|AV_CH_LOW_FREQUENCY,
    AV_CH_LAYOUT_QUAD,
    0,
    AV_CH_LAYOUT_5POINT1_BACK,
    AV_CH_LAYOUT_5POINT1_BACK|AV_CH_BACK_CENTER,
    AV_CH_LAYOUT_7POINT1_WIDE
};

static av_cold int tta_rs_decode_init(AVCodecContext *avctx)
{
    TtaRsContext *s = avctx->priv_data;
    int ret;

    ret = tta_rs_new(avctx->extradata,
                     avctx->extradata_size > 0 ? avctx->extradata_size : 0,
                     (const uint8_t *)s->pass, s->pass ? strlen(s->pass) : 0,
                     &s->info, &s->dec);
    switch (ret) {
    case 0:
        break;
    case 1:                     /* fewer than 22 bytes for a TTA1 header */
        return AVERROR_INVALIDDATA;
    case 2:
        av_log(avctx, AV_LOG_ERROR, "Wrong extradata present\n");
        return AVERROR_INVALIDDATA;
    case 3:
        av_log(avctx, AV_LOG_ERROR, "Invalid format\n");
        return AVERROR_INVALIDDATA;
    case 4:
        av_log(avctx, AV_LOG_ERROR, "Missing password for encrypted stream. Please use the -password option\n");
        return AVERROR(EINVAL);
    case 5:
        av_log(avctx, AV_LOG_ERROR, "Invalid number of channels\n");
        return AVERROR_INVALIDDATA;
    case 6:
        av_log(avctx, AV_LOG_ERROR, "Invalid samplerate\n");
        return AVERROR_INVALIDDATA;
    case 7:
        av_log(avctx, AV_LOG_ERROR, "Invalid/unsupported sample format.\n");
        return AVERROR_INVALIDDATA;
    case 8:
        av_log(avctx, AV_LOG_ERROR, "sample_rate too large\n");
        return AVERROR(EINVAL);
    case 9:
        av_log(avctx, AV_LOG_ERROR, "frame_length too large\n");
        return AVERROR_INVALIDDATA;
    default:
        return AVERROR_BUG;
    }

    av_channel_layout_uninit(&avctx->ch_layout);
    if (s->info.channels > 1 && s->info.channels < 9)
        av_channel_layout_from_mask(&avctx->ch_layout,
                                    tta_rs_channel_layouts[s->info.channels - 2]);
    if (avctx->ch_layout.nb_channels == 0) {
        avctx->ch_layout.order       = AV_CHANNEL_ORDER_UNSPEC;
        avctx->ch_layout.nb_channels = s->info.channels;
    }

    avctx->bits_per_raw_sample = s->info.bits_per_raw_sample;
    avctx->sample_rate         = s->info.sample_rate;

    switch (s->info.bps) {
    case 1: avctx->sample_fmt = AV_SAMPLE_FMT_U8;  break;
    case 2: avctx->sample_fmt = AV_SAMPLE_FMT_S16; break;
    case 3: avctx->sample_fmt = AV_SAMPLE_FMT_S32; break;
    default:
        return AVERROR_BUG;
    }

    return 0;
}

static int tta_rs_decode_frame(AVCodecContext *avctx, AVFrame *frame,
                               int *got_frame_ptr, AVPacket *avpkt)
{
    TtaRsContext *s = avctx->priv_data;
    int buf_size = avpkt->size;
    size_t out_bytes;
    int ret;

    if (avctx->err_recognition & AV_EF_CRCCHECK) {
        if (buf_size < 4)
            return AVERROR_INVALIDDATA;
        ret = tta_rs_crc_ok(avpkt->data, buf_size);
        if (ret < 0)
            return AVERROR_BUG;
        if (!ret) {
            av_log(avctx, AV_LOG_ERROR, "CRC error\n");
            if (avctx->err_recognition & AV_EF_EXPLODE)
                return AVERROR_INVALIDDATA;
        }
    }

    /* What init_get_bits8() refuses in the C decoder. */
    if (!avpkt->data || buf_size < 0 || buf_size > INT_MAX / 8 ||
        buf_size * 8 >= INT_MAX - AV_INPUT_BUFFER_PADDING_SIZE * 8)
        return AVERROR_INVALIDDATA;

    /* get output buffer */
    frame->nb_samples = s->info.frame_length;
    if ((ret = ff_thread_get_buffer(avctx, frame, 0)) < 0)
        return ret;

    /* Rust is given the samples of a full frame and nothing else. */
    out_bytes = (size_t)s->info.frame_length * s->info.channels *
                (s->info.bps == 3 ? 4 : s->info.bps);
    if (frame->linesize[0] < 0 || (size_t)frame->linesize[0] < out_bytes)
        return AVERROR_BUG;

    ret = tta_rs_decode(s->dec, avpkt->data, buf_size, frame->data[0], out_bytes);
    if (ret == -2)
        return AVERROR_INVALIDDATA;
    if (ret < 0)
        return AVERROR_BUG;

    /* the stream's last frame is shorter */
    frame->nb_samples = ret;

    *got_frame_ptr = 1;

    return buf_size;
}

static av_cold int tta_rs_decode_close(AVCodecContext *avctx)
{
    TtaRsContext *s = avctx->priv_data;

    tta_rs_free(s->dec);
    s->dec = NULL;

    return 0;
}

#define OFFSET(x) offsetof(TtaRsContext, x)
#define DEC (AV_OPT_FLAG_DECODING_PARAM | AV_OPT_FLAG_AUDIO_PARAM)
static const AVOption tta_rs_options[] = {
    { "password", "Set decoding password", OFFSET(pass), AV_OPT_TYPE_STRING, { .str = NULL }, 0, 0, DEC },
    { NULL },
};

static const AVClass tta_rs_decoder_class = {
    .class_name = "TTA Decoder (Rust)",
    .item_name  = av_default_item_name,
    .option     = tta_rs_options,
    .version    = LIBAVUTIL_VERSION_INT,
};

const FFCodec ff_tta_rs_decoder = {
    .p.name         = "tta_rs",
    CODEC_LONG_NAME("TTA (True Audio), safe Rust port"),
    .p.type         = AVMEDIA_TYPE_AUDIO,
    .p.id           = AV_CODEC_ID_TTA,
    .priv_data_size = sizeof(TtaRsContext),
    .init           = tta_rs_decode_init,
    .close          = tta_rs_decode_close,
    FF_CODEC_DECODE_CB(tta_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1 | AV_CODEC_CAP_FRAME_THREADS | AV_CODEC_CAP_CHANNEL_CONF,
    .p.priv_class   = &tta_rs_decoder_class,
    .caps_internal  = FF_CODEC_CAP_INIT_CLEANUP,
};
