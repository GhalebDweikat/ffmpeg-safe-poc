/*
 * Apple Intermediate Codec decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of aic.c as the decoder "aic_rs".
 *
 * This file only talks to the FFmpeg API: it owns the Rust decoder object,
 * allocates the frame, hands the packet and the three visible planes to
 * Rust as (pointer, length) pairs, and maps the result to an FFmpeg return
 * code. It never reads packet bytes or writes samples itself.
 */

#include <stddef.h>
#include <stdint.h>

#include "config.h"
#include "config_components.h"

#include "libavutil/attributes.h"
#include "libavutil/cpu.h"
#include "libavutil/internal.h"
#if ARCH_X86
#include "libavutil/x86/cpu.h"
#endif
#include "avcodec.h"
#include "codec_internal.h"
#include "thread.h"

typedef struct AicRs AicRs;

typedef struct AicRsError {
    uint32_t a, b;
} AicRsError;

AicRs *aic_rs_new(int width, int height, int x86_idct);
void aic_rs_free(AicRs *dec);
int aic_rs_check(const AicRs *dec, const uint8_t *pkt, size_t pkt_size,
                 int width, int height, AicRsError *info);
int aic_rs_decode(AicRs *dec, const uint8_t *pkt, size_t pkt_size,
                  uint8_t *y, size_t y_len, size_t y_stride,
                  uint8_t *u, size_t u_len, size_t u_stride,
                  uint8_t *v, size_t v_len, size_t v_stride,
                  int width, int height, AicRsError *info);

typedef struct AicRsContext {
    AicRs *dec;
} AicRsContext;

/* Which inverse DCT ff_idctdsp_init() would give the C decoder. The port
 * has the 8-bit "simple" IDCT in two forms that differ only when
 * intermediate values overflow 16 bits: as the C function computes it, and
 * as the x86-64 assembly does. Returns 0 for the C form, 1 for the x86
 * form, or -1 for an IDCT that is not ported. */
static int which_idct(const AVCodecContext *avctx)
{
    int x86 = 0;

    if (avctx->lowres ||
        avctx->bits_per_raw_sample == 9  || avctx->bits_per_raw_sample == 10 ||
        avctx->bits_per_raw_sample == 12 ||
        avctx->idct_algo == FF_IDCT_INT
#if CONFIG_FAANIDCT
        || avctx->idct_algo == FF_IDCT_FAAN
#endif
#if CONFIG_MPEG4_DECODER
        || avctx->idct_algo == FF_IDCT_XVID
#endif
        )
        return -1;

#if ARCH_X86_64 && HAVE_X86ASM
    if (EXTERNAL_SSE2(av_get_cpu_flags()) && avctx->bits_per_raw_sample <= 8 &&
        (avctx->idct_algo == FF_IDCT_AUTO       ||
         avctx->idct_algo == FF_IDCT_SIMPLEAUTO ||
         avctx->idct_algo == FF_IDCT_SIMPLEMMX  ||
         avctx->idct_algo == FF_IDCT_SIMPLE))
        x86 = 1;
#endif
    /* Other architectures have assembly IDCTs of their own. They were not
     * compared; the C form is used there. */
    return x86;
}

static av_cold int aic_rs_decode_init(AVCodecContext *avctx)
{
    AicRsContext *ctx = avctx->priv_data;
    int idct = which_idct(avctx);

    avctx->pix_fmt = AV_PIX_FMT_YUV420P;

    if (idct < 0) {
        avpriv_report_missing_feature(avctx, "An IDCT other than the 8-bit simple IDCT (C or x86-64)");
        return AVERROR_PATCHWELCOME;
    }

    ctx->dec = aic_rs_new(avctx->width, avctx->height, idct);
    if (!ctx->dec) {
        av_log(avctx, AV_LOG_ERROR, "Error allocating slice buffer\n");
        return AVERROR(ENOMEM);
    }

    return 0;
}

/* Returns AVERROR_INVALIDDATA after logging what the C decoder logs. */
static int report(AVCodecContext *avctx, int status, const AicRsError *e)
{
    switch (status) {
    case 1:
        av_log(avctx, AV_LOG_ERROR, "Too small frame\n");
        return AVERROR_INVALIDDATA;
    case 2:
        av_log(avctx, AV_LOG_ERROR, "Invalid version %d\n", (int)e->a);
        break;
    case 3:
        av_log(avctx, AV_LOG_ERROR, "Invalid header size %d\n", (int)e->a);
        break;
    case 4:
        av_log(avctx, AV_LOG_ERROR, "Frame size should be %"PRIu32" got more\n", e->a);
        break;
    case 5:
        av_log(avctx, AV_LOG_ERROR,
               "Picture dimension changed: old: %d x %d, new: %d x %d\n",
               avctx->width, avctx->height, (int)e->a, (int)e->b);
        break;
    case 6:
        av_log(avctx, AV_LOG_ERROR, "Incorrect slice size at %d.%d\n", (int)e->a, (int)e->b);
        return AVERROR_INVALIDDATA;
    case 7:
        av_log(avctx, AV_LOG_ERROR, "Error decoding slice at %d.%d\n", (int)e->a, (int)e->b);
        return AVERROR_INVALIDDATA;
    default:
        return AVERROR_BUG;
    }
    av_log(avctx, AV_LOG_ERROR, "Invalid header\n");
    return AVERROR_INVALIDDATA;
}

static int aic_rs_decode_frame(AVCodecContext *avctx, AVFrame *frame,
                               int *got_frame, AVPacket *avpkt)
{
    AicRsContext *ctx = avctx->priv_data;
    AicRsError info   = { 0 };
    size_t cw, ch;
    int ret;

    ret = aic_rs_check(ctx->dec, avpkt->data, avpkt->size,
                       avctx->width, avctx->height, &info);
    if (ret)
        return report(avctx, ret, &info);

    if ((ret = ff_thread_get_buffer(avctx, frame, 0)) < 0)
        return ret;

    /* Rust is given the visible samples of each plane and nothing else:
     * rows - 1 full rows plus one visible row. */
    cw = ((size_t)avctx->width  + 1) / 2;
    ch = ((size_t)avctx->height + 1) / 2;
    if (avctx->width <= 0 || avctx->height <= 0 ||
        frame->width < avctx->width || frame->height < avctx->height ||
        frame->linesize[0] < avctx->width ||
        frame->linesize[1] < (ptrdiff_t)cw || frame->linesize[2] < (ptrdiff_t)cw)
        return AVERROR_BUG;

    ret = aic_rs_decode(ctx->dec, avpkt->data, avpkt->size,
                        frame->data[0],
                        (size_t)frame->linesize[0] * (avctx->height - 1) + avctx->width,
                        frame->linesize[0],
                        frame->data[1], (size_t)frame->linesize[1] * (ch - 1) + cw,
                        frame->linesize[1],
                        frame->data[2], (size_t)frame->linesize[2] * (ch - 1) + cw,
                        frame->linesize[2],
                        avctx->width, avctx->height, &info);
    if (ret)
        return report(avctx, ret, &info);

    *got_frame = 1;

    return avpkt->size;
}

static av_cold int aic_rs_decode_close(AVCodecContext *avctx)
{
    AicRsContext *ctx = avctx->priv_data;

    aic_rs_free(ctx->dec);
    ctx->dec = NULL;

    return 0;
}

const FFCodec ff_aic_rs_decoder = {
    .p.name         = "aic_rs",
    CODEC_LONG_NAME("Apple Intermediate Codec, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_AIC,
    .priv_data_size = sizeof(AicRsContext),
    .init           = aic_rs_decode_init,
    .close          = aic_rs_decode_close,
    FF_CODEC_DECODE_CB(aic_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1 | AV_CODEC_CAP_FRAME_THREADS,
    .caps_internal  = FF_CODEC_CAP_INIT_CLEANUP,
};
