/*
 * Apple ProRes decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of proresdec.c as the decoder "prores_rs".
 *
 * This file only talks to the FFmpeg API: it lets Rust read the frame
 * header, applies what the header says to the codec context and the frame
 * in the order the C decoder does, allocates the frame, hands the packet
 * and the visible part of each plane to Rust as (pointer, length) pairs,
 * and maps the result to an FFmpeg return code. It never reads packet
 * bytes or writes samples itself.
 *
 * Unlike the C decoder it does not decode slices or frames in parallel.
 */

#include <stddef.h>
#include <stdint.h>

#include "config.h"

#include "libavutil/cpu.h"
#include "libavutil/internal.h"
#if ARCH_X86_64 && HAVE_X86ASM
#include "libavutil/x86/cpu.h"
#endif
#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"
#include "profiles.h"

typedef struct ProresRsHeader {
    int width;
    int height;
    uint8_t frame_type;         /* 0 progressive, 1 tff, 2 and 3 bff */
    uint8_t alpha_info;
    uint8_t chroma444;
    uint8_t color_primaries;
    uint8_t color_trc;
    uint8_t colorspace;
    uint8_t truncated;
} ProresRsHeader;

int prores_rs_header(const uint8_t *pkt, size_t pkt_size, ProresRsHeader *out);
int prores_rs_decode(const uint8_t *pkt, size_t pkt_size,
                     uint8_t *const planes[4], const size_t lens[4],
                     const size_t strides[4], int idct, int skip_alpha,
                     int gray, int top_field_first);

/* Which of FFmpeg's inverse DCTs the Rust code has to reproduce. They
 * differ on extreme coefficients. */
enum {
    IDCT_C_10   = 0,
    IDCT_X86_10 = 1,
    IDCT_C_12   = 2,
};

typedef struct ProresRsContext {
    int frame_type;             /* of the previous frame header */
    enum AVPixelFormat pix_fmt;
    int idct;
} ProresRsContext;

static av_cold int prores_rs_decode_init(AVCodecContext *avctx)
{
    ProresRsContext *s = avctx->priv_data;

    avctx->bits_per_raw_sample = 10;

    switch (avctx->codec_tag) {
    case MKTAG('a','p','c','o'):
        avctx->profile = AV_PROFILE_PRORES_PROXY;
        break;
    case MKTAG('a','p','c','s'):
        avctx->profile = AV_PROFILE_PRORES_LT;
        break;
    case MKTAG('a','p','c','n'):
        avctx->profile = AV_PROFILE_PRORES_STANDARD;
        break;
    case MKTAG('a','p','c','h'):
        avctx->profile = AV_PROFILE_PRORES_HQ;
        break;
    case MKTAG('a','p','4','h'):
        avctx->profile = AV_PROFILE_PRORES_4444;
        avctx->bits_per_raw_sample = 12;
        break;
    case MKTAG('a','p','4','x'):
        avctx->profile = AV_PROFILE_PRORES_XQ;
        avctx->bits_per_raw_sample = 12;
        break;
    default:
        avctx->profile = AV_PROFILE_UNKNOWN;
        av_log(avctx, AV_LOG_WARNING, "Unknown prores profile %d\n", avctx->codec_tag);
    }

    /* The choice ff_proresdsp_init() makes: assembly for 10 bits on
     * x86-64 when the build and the CPU have it, C otherwise. */
    s->idct = avctx->bits_per_raw_sample == 10 ? IDCT_C_10 : IDCT_C_12;
#if ARCH_X86_64 && HAVE_X86ASM
    if (avctx->bits_per_raw_sample == 10) {
        int cpu_flags = av_get_cpu_flags();
        if (EXTERNAL_SSE2(cpu_flags) || EXTERNAL_AVX(cpu_flags))
            s->idct = IDCT_X86_10;
    }
#endif

    s->pix_fmt = AV_PIX_FMT_NONE;

    return 0;
}

static int prores_rs_decode_frame(AVCodecContext *avctx, AVFrame *frame,
                                  int *got_frame, AVPacket *avpkt)
{
    ProresRsContext *s = avctx->priv_data;
    ProresRsHeader hdr = { 0 };
    enum AVPixelFormat pix_fmt;
    uint8_t *planes[4];
    size_t lens[4], strides[4], widths[4];
    int old_frame_type = s->frame_type;
    int alpha_info, dimensions_changed = 0;
    int ret;

    ret = prores_rs_header(avpkt->data, avpkt->size, &hdr);
    switch (ret) {
    case 0:
        break;
    case 1:
        av_log(avctx, AV_LOG_ERROR, "invalid frame header\n");
        return AVERROR_INVALIDDATA;
    case 2:
        av_log(avctx, AV_LOG_ERROR, "unsupported version\n");
        return AVERROR_PATCHWELCOME;
    default:
        return AVERROR_BUG;
    }

    if (hdr.width != avctx->width || hdr.height != avctx->height) {
        av_log(avctx, AV_LOG_WARNING, "picture resolution change: %dx%d -> %dx%d\n",
               avctx->width, avctx->height, hdr.width, hdr.height);
        if ((ret = ff_set_dimensions(avctx, hdr.width, hdr.height)) < 0)
            return ret;
        dimensions_changed = 1;
    }

    s->frame_type = hdr.frame_type;
    alpha_info    = hdr.alpha_info;

    if (alpha_info > 2) {
        av_log(avctx, AV_LOG_ERROR, "Invalid alpha mode %d\n", alpha_info);
        return AVERROR_INVALIDDATA;
    }
    if (avctx->skip_alpha) alpha_info = 0;

    if (s->frame_type != 0) {
        frame->flags |= AV_FRAME_FLAG_INTERLACED;
        if (s->frame_type == 1)
            frame->flags |= AV_FRAME_FLAG_TOP_FIELD_FIRST;
    }

    if (alpha_info) {
        if (avctx->bits_per_raw_sample == 10) {
            pix_fmt = hdr.chroma444 ? AV_PIX_FMT_YUVA444P10 : AV_PIX_FMT_YUVA422P10;
        } else { /* 12b */
            pix_fmt = hdr.chroma444 ? AV_PIX_FMT_YUVA444P12 : AV_PIX_FMT_YUVA422P12;
        }
    } else {
        if (avctx->bits_per_raw_sample == 10) {
            pix_fmt = hdr.chroma444 ? AV_PIX_FMT_YUV444P10 : AV_PIX_FMT_YUV422P10;
        } else { /* 12b */
            pix_fmt = hdr.chroma444 ? AV_PIX_FMT_YUV444P12 : AV_PIX_FMT_YUV422P12;
        }
    }

    if (pix_fmt != s->pix_fmt || dimensions_changed ||
        s->frame_type != old_frame_type)
        avctx->pix_fmt = s->pix_fmt = pix_fmt;

    avctx->color_primaries = hdr.color_primaries;
    avctx->color_trc       = hdr.color_trc;
    avctx->colorspace      = hdr.colorspace;
    avctx->color_range     = AVCOL_RANGE_MPEG;

    if (hdr.truncated) {
        av_log(avctx, AV_LOG_ERROR, "Header truncated\n");
        return AVERROR_INVALIDDATA;
    }

    if (avctx->skip_frame == AVDISCARD_ALL)
        return 0;

    if ((ret = ff_get_buffer(avctx, frame, 0)) < 0)
        return ret;

    /* Rust is given the visible samples of each plane and nothing else:
     * height - 1 full rows plus one visible row, in 16-bit units. */
    widths[0] = widths[3] = avctx->width;
    widths[1] = widths[2] = hdr.chroma444 ? avctx->width : ((size_t)avctx->width + 1) / 2;
    if (avctx->width != hdr.width || avctx->height != hdr.height ||
        avctx->width <= 0 || avctx->height <= 0)
        return AVERROR_BUG;
    for (int p = 0; p < 4; p++) {
        if (p == 3 && !alpha_info) {
            planes[p]  = NULL;
            lens[p]    = 0;
            strides[p] = 0;
            continue;
        }
        if (!frame->data[p] || (frame->linesize[p] & 1) ||
            frame->linesize[p] < 2 * (ptrdiff_t)widths[p])
            return AVERROR_BUG;
        planes[p]  = frame->data[p];
        strides[p] = frame->linesize[p] / 2;
        lens[p]    = strides[p] * (avctx->height - 1) + widths[p];
    }

    ret = prores_rs_decode(avpkt->data, avpkt->size, planes, lens, strides,
                           s->idct, !!avctx->skip_alpha,
                           !!(avctx->flags & AV_CODEC_FLAG_GRAY),
                           !!(frame->flags & AV_FRAME_FLAG_TOP_FIELD_FIRST));
    switch (ret) {
    case 0:
        break;
    case 1:                     /* some slices are missing */
        frame->decode_error_flags = FF_DECODE_ERROR_INVALID_BITSTREAM;
        break;
    case 2:
        av_log(avctx, AV_LOG_ERROR, "error decoding picture header\n");
        return AVERROR_INVALIDDATA;
    case 3:
        av_log(avctx, AV_LOG_ERROR, "error decoding picture header\n");
        return AVERROR(EINVAL);
    case 4:
        /* As in the C decoder: when every slice of a picture fails, the
         * result is the -1 each slice starts out with. */
        frame->decode_error_flags = FF_DECODE_ERROR_INVALID_BITSTREAM;
        av_log(avctx, AV_LOG_ERROR, "error decoding picture\n");
        return -1;
    default:
        return AVERROR_BUG;
    }

    *got_frame = 1;

    return avpkt->size;
}

const FFCodec ff_prores_rs_decoder = {
    .p.name         = "prores_rs",
    CODEC_LONG_NAME("Apple ProRes (iCodec Pro), safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_PRORES,
    .priv_data_size = sizeof(ProresRsContext),
    .init           = prores_rs_decode_init,
    FF_CODEC_DECODE_CB(prores_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1,
    .caps_internal  = FF_CODEC_CAP_SKIP_FRAME_FILL_PARAM,
    .p.profiles     = NULL_IF_CONFIG_SMALL(ff_prores_profiles),
};
