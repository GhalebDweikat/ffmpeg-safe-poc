/*
 * FLAC (Free Lossless Audio Codec) decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of flacdec.c as the decoder "flac_rs".
 *
 * This file only talks to the FFmpeg API: it owns the Rust decoder object,
 * copies the stream parameters the decoder finds into the codec context,
 * allocates the frame, hands the packet and the frame's samples to Rust as
 * (pointer, length) pairs, and maps the result to an FFmpeg return code.
 * It never reads packet bytes or writes samples itself.
 *
 * Unlike the C decoder it does not declare frame threading.
 */

#include <stddef.h>
#include <stdint.h>

#include "config.h"

#include "libavutil/channel_layout.h"
#include "libavutil/cpu.h"
#include "libavutil/internal.h"
#include "libavutil/opt.h"
#include "libavutil/samplefmt.h"
#if ARCH_X86
#include "libavutil/x86/cpu.h"
#endif
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"

typedef struct FlacRs FlacRs;

typedef struct FlacRsUpdates {
    int32_t channels;               /* 0: leave the layout alone */
    int32_t sample_rate;            /* -1: none */
    int32_t bits_per_raw_sample;    /* -1: none */
    int32_t sample_fmt;             /* 0 S16, 1 S32, 2 S16P, 3 S32P; -1: none */
} FlacRsUpdates;

typedef struct FlacRsFrame {
    FlacRsUpdates updates;
    int32_t consumed;
    int32_t nb_samples;
    int32_t channels;
    int32_t crc_error;
} FlacRsFrame;

enum { FLAC_RS_FRAME = 0, FLAC_RS_SKIP = 1, FLAC_RS_INVALID = 2 };

#define FLAC_RS_WANT32    1u
#define FLAC_RS_PLANAR    2u
#define FLAC_RS_CHECK_CRC 4u
#define FLAC_RS_BUGGY_LPC 8u

#define FLAC_RS_SATURATE_STEREO 1u
#define FLAC_RS_SATURATE_MULTI  2u
#define FLAC_RS_SATURATE_8      4u

FlacRs *flac_rs_new(const uint8_t *extradata, size_t extradata_size, int has_extradata,
                    uint32_t options, FlacRsUpdates *upd, int *err);
void flac_rs_free(FlacRs *dec);
int flac_rs_decode(FlacRs *dec, const uint8_t *pkt, size_t pkt_size, uint32_t options,
                   FlacRsFrame *out);
int flac_rs_output(FlacRs *dec, uint8_t *const *planes, int nb_planes, size_t plane_bytes,
                   int fmt, uint32_t quirks);

typedef struct FlacRsContext {
    AVClass *class;
    FlacRs *dec;
    int buggy_lpc;                  ///< use workaround for old lavc encoded files
} FlacRsContext;

static const enum AVSampleFormat flac_rs_sample_fmts[4] = {
    AV_SAMPLE_FMT_S16, AV_SAMPLE_FMT_S32, AV_SAMPLE_FMT_S16P, AV_SAMPLE_FMT_S32P,
};

static const AVChannelLayout flac_rs_channel_layouts[8] = {
    AV_CHANNEL_LAYOUT_MONO,
    AV_CHANNEL_LAYOUT_STEREO,
    AV_CHANNEL_LAYOUT_SURROUND,
    AV_CHANNEL_LAYOUT_QUAD,
    AV_CHANNEL_LAYOUT_5POINT0,
    AV_CHANNEL_LAYOUT_5POINT1,
    AV_CHANNEL_LAYOUT_6POINT1,
    AV_CHANNEL_LAYOUT_7POINT1
};

/* As ff_flac_set_channel_layout: a layout the caller set is kept as long
 * as its channel count fits. */
static void flac_rs_set_channel_layout(AVCodecContext *avctx, int channels)
{
    if (channels == avctx->ch_layout.nb_channels &&
        avctx->ch_layout.order != AV_CHANNEL_ORDER_UNSPEC)
        return;

    av_channel_layout_uninit(&avctx->ch_layout);
    if (channels >= 1 && channels <= FF_ARRAY_ELEMS(flac_rs_channel_layouts))
        avctx->ch_layout = flac_rs_channel_layouts[channels - 1];
    else
        avctx->ch_layout = (AVChannelLayout){ .order = AV_CHANNEL_ORDER_UNSPEC,
                                              .nb_channels = channels };
}

static void flac_rs_apply(AVCodecContext *avctx, const FlacRsUpdates *u)
{
    if (u->sample_rate >= 0)
        avctx->sample_rate = u->sample_rate;
    if (u->bits_per_raw_sample >= 0)
        avctx->bits_per_raw_sample = u->bits_per_raw_sample;
    if (u->channels > 0)
        flac_rs_set_channel_layout(avctx, u->channels);
    if (u->sample_fmt >= 0 && u->sample_fmt < FF_ARRAY_ELEMS(flac_rs_sample_fmts))
        avctx->sample_fmt = flac_rs_sample_fmts[u->sample_fmt];
}

static uint32_t flac_rs_options(const AVCodecContext *avctx)
{
    const FlacRsContext *s = avctx->priv_data;
    enum AVSampleFormat req = avctx->request_sample_fmt;
    uint32_t options = 0;

    if (av_get_bytes_per_sample(req) > 2)
        options |= FLAC_RS_WANT32;
    if (av_sample_fmt_is_planar(req))
        options |= FLAC_RS_PLANAR;
    if (avctx->err_recognition & (AV_EF_CRCCHECK | AV_EF_COMPLIANT))
        options |= FLAC_RS_CHECK_CRC;
    if (s->buggy_lpc)
        options |= FLAC_RS_BUGGY_LPC;
    return options;
}

/* FFmpeg's x86 assembly for interleaved 16-bit output narrows samples by
 * saturation, its C code by truncation; a damaged stream shows the
 * difference. The port does what the C decoder of this build does on this
 * CPU (see ff_flacdsp_init_x86). */
static uint32_t flac_rs_quirks(void)
{
    uint32_t quirks = 0;
#if ARCH_X86 && HAVE_X86ASM
    int cpu_flags = av_get_cpu_flags();

    if (EXTERNAL_SSE2(cpu_flags))
        quirks |= FLAC_RS_SATURATE_STEREO;
    if (EXTERNAL_SSSE3(cpu_flags)) {
        quirks |= FLAC_RS_SATURATE_MULTI;
#if ARCH_X86_64
        quirks |= FLAC_RS_SATURATE_8;
#endif
    }
#if ARCH_X86_64
    if (EXTERNAL_AVX(cpu_flags))
        quirks |= FLAC_RS_SATURATE_8;
#endif
#endif
    return quirks;
}

static av_cold int flac_rs_decode_init(AVCodecContext *avctx)
{
    FlacRsContext *s = avctx->priv_data;
    FlacRsUpdates upd;
    int err = 0;

    /* Without extradata the stream parameters come from the packets. */
    s->dec = flac_rs_new(avctx->extradata,
                         avctx->extradata_size > 0 ? avctx->extradata_size : 0,
                         avctx->extradata != NULL, flac_rs_options(avctx), &upd, &err);
    if (!s->dec) {
        if (err == 1) {
            av_log(avctx, AV_LOG_ERROR, "invalid extradata\n");
            return AVERROR_INVALIDDATA;
        }
        return AVERROR(ENOMEM);
    }
    flac_rs_apply(avctx, &upd);

    return 0;
}

static int flac_rs_decode_frame(AVCodecContext *avctx, AVFrame *frame,
                                int *got_frame_ptr, AVPacket *avpkt)
{
    FlacRsContext *s = avctx->priv_data;
    FlacRsFrame f;
    uint8_t *planes[8];
    size_t plane_bytes;
    int ret, fmt, planar, nb_planes;

    *got_frame_ptr = 0;

    ret = flac_rs_decode(s->dec, avpkt->data, avpkt->size, flac_rs_options(avctx), &f);
    if (ret < 0)
        return AVERROR_BUG;
    /* What the decoder learned about the stream holds even if the packet
     * is rejected. */
    flac_rs_apply(avctx, &f.updates);
    if (ret == FLAC_RS_SKIP)
        return f.consumed;
    if (ret == FLAC_RS_INVALID) {
        av_log(avctx, AV_LOG_ERROR, "invalid data\n");
        return AVERROR_INVALIDDATA;
    }

    if (f.crc_error) {
        av_log(avctx, AV_LOG_ERROR, "CRC error at PTS %"PRId64"\n", avpkt->pts);
        if (avctx->err_recognition & AV_EF_EXPLODE)
            return AVERROR_INVALIDDATA;
    }

    /* get output buffer */
    frame->nb_samples = f.nb_samples;
    if ((ret = ff_get_buffer(avctx, frame, 0)) < 0)
        return ret;

    /* Rust is given the frame's samples and nothing else. */
    for (fmt = 0; fmt < FF_ARRAY_ELEMS(flac_rs_sample_fmts); fmt++)
        if (flac_rs_sample_fmts[fmt] == avctx->sample_fmt)
            break;
    if (fmt == FF_ARRAY_ELEMS(flac_rs_sample_fmts) || frame->format != avctx->sample_fmt ||
        frame->nb_samples != f.nb_samples || frame->ch_layout.nb_channels != f.channels ||
        f.channels < 1 || f.channels > 8)
        return AVERROR_BUG;
    planar      = av_sample_fmt_is_planar(frame->format);
    nb_planes   = planar ? f.channels : 1;
    plane_bytes = (size_t)f.nb_samples * av_get_bytes_per_sample(frame->format) *
                  (planar ? 1 : f.channels);
    for (int i = 0; i < nb_planes; i++)
        planes[i] = frame->extended_data[i];

    ret = flac_rs_output(s->dec, planes, nb_planes, plane_bytes, fmt, flac_rs_quirks());
    if (ret < 0)
        return AVERROR_BUG;

    if (f.consumed > avpkt->size) {
        av_log(avctx, AV_LOG_ERROR, "overread: %d\n", f.consumed - avpkt->size);
        return AVERROR_INVALIDDATA;
    }

    *got_frame_ptr = 1;

    return f.consumed;
}

static av_cold int flac_rs_decode_close(AVCodecContext *avctx)
{
    FlacRsContext *s = avctx->priv_data;

    flac_rs_free(s->dec);
    s->dec = NULL;

    return 0;
}

static const AVOption flac_rs_options_table[] = {
{ "use_buggy_lpc", "emulate old buggy lavc behavior", offsetof(FlacRsContext, buggy_lpc), AV_OPT_TYPE_BOOL, {.i64 = 0 }, 0, 1, AV_OPT_FLAG_DECODING_PARAM | AV_OPT_FLAG_AUDIO_PARAM },
{ NULL },
};

static const AVClass flac_rs_decoder_class = {
    .class_name = "FLAC decoder (Rust)",
    .item_name  = av_default_item_name,
    .option     = flac_rs_options_table,
    .version    = LIBAVUTIL_VERSION_INT,
};

const FFCodec ff_flac_rs_decoder = {
    .p.name         = "flac_rs",
    CODEC_LONG_NAME("FLAC (Free Lossless Audio Codec), safe Rust port"),
    .p.type         = AVMEDIA_TYPE_AUDIO,
    .p.id           = AV_CODEC_ID_FLAC,
    .priv_data_size = sizeof(FlacRsContext),
    .init           = flac_rs_decode_init,
    .close          = flac_rs_decode_close,
    FF_CODEC_DECODE_CB(flac_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_CHANNEL_CONF |
                      AV_CODEC_CAP_DR1,
    CODEC_SAMPLEFMTS(AV_SAMPLE_FMT_S16, AV_SAMPLE_FMT_S16P,
                     AV_SAMPLE_FMT_S32, AV_SAMPLE_FMT_S32P),
    .p.priv_class   = &flac_rs_decoder_class,
};
