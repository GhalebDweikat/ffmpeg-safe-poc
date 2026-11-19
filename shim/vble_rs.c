/*
 * VBLE Lossless Codec decoder, safe Rust port: FFmpeg glue.
 *
 * This file is free software under the same terms as FFmpeg (LGPL 2.1+).
 */

/**
 * @file
 * Registers the Rust port of vble.c as the decoder "vble_rs".
 *
 * This file only talks to the FFmpeg API: it owns the Rust decoder object,
 * allocates the frame, hands the packet and the three visible planes to
 * Rust as (pointer, length) pairs, and maps the result to an FFmpeg return
 * code. It never reads packet bytes or writes samples itself.
 */

#include <stddef.h>
#include <stdint.h>

#include "config.h"
#include "libavutil/common.h"
#include "libavutil/cpu.h"
#include "libavutil/imgutils.h"
#include "libavutil/internal.h"
#if ARCH_X86
#include "libavutil/x86/cpu.h"
#endif
#include "avcodec.h"
#include "codec_internal.h"
#include "thread.h"

typedef struct VbleRs VbleRs;

#define VBLE_RS_GRAY           1
#define VBLE_RS_SSE2_OVERWRITE 2

VbleRs *vble_rs_new(int width, int height);
void vble_rs_free(VbleRs *dec);
size_t vble_rs_symbols(const VbleRs *dec);
int vble_rs_unpack(VbleRs *dec, const uint8_t *pkt, size_t pkt_size, uint32_t *version);
int vble_rs_restore(VbleRs *dec, const uint8_t *pkt, size_t pkt_size,
                    uint8_t *y, size_t y_len, size_t y_stride,
                    uint8_t *u, size_t u_len, size_t u_stride,
                    uint8_t *v, size_t v_len, size_t v_stride,
                    int width, int height, uint32_t flags);

typedef struct VbleRsContext {
    VbleRs *dec;
    int sse2_overwrite;
} VbleRsContext;

static av_cold int vble_rs_decode_init(AVCodecContext *avctx)
{
    VbleRsContext *s = avctx->priv_data;
    int size;

    avctx->pix_fmt = AV_PIX_FMT_YUV420P;
    avctx->bits_per_raw_sample = 8;

    size = av_image_get_buffer_size(avctx->pix_fmt,
                                    avctx->width, avctx->height, 1);
    if (size < 0)
        return size;

    s->dec = vble_rs_new(avctx->width, avctx->height);
    if (!s->dec) {
        av_log(avctx, AV_LOG_ERROR, "Could not allocate values buffer.\n");
        return AVERROR(ENOMEM);
    }
    if (vble_rs_symbols(s->dec) != size)
        return AVERROR_BUG;

    /* The C decoder's output for odd widths depends on which median
     * predictor ff_llviddsp_init() picks: the SSE2 one stores whole blocks
     * of 16 samples, the C one stops at the end of the row. Tell Rust
     * which one this build of FFmpeg would use on this CPU. */
#if ARCH_X86 && HAVE_X86ASM
    s->sse2_overwrite = !!EXTERNAL_SSE2(av_get_cpu_flags());
#endif

    return 0;
}

static int vble_rs_decode_frame(AVCodecContext *avctx, AVFrame *pic,
                                int *got_frame, AVPacket *avpkt)
{
    VbleRsContext *s = avctx->priv_data;
    int width = avctx->width, height = avctx->height;
    int cw = AV_CEIL_RSHIFT(width, 1), ch = AV_CEIL_RSHIFT(height, 1);
    uint32_t version = 1, flags = 0;
    int ret;

    ret = vble_rs_unpack(s->dec, avpkt->data, avpkt->size, &version);
    switch (ret) {
    case 0:
        break;
    case 1:
        av_log(avctx, AV_LOG_ERROR, "Invalid packet size\n");
        return AVERROR_INVALIDDATA;
    case 2:
        av_log(avctx, AV_LOG_ERROR, "Invalid Code\n");
        return AVERROR_INVALIDDATA;
    default:
        return AVERROR_BUG;
    }
    /* Version should always be 1 */
    if (version != 1)
        av_log(avctx, AV_LOG_WARNING, "Unsupported VBLE Version: %d\n", (int)version);

    /* Allocate buffer */
    if ((ret = ff_thread_get_buffer(avctx, pic, 0)) < 0)
        return ret;

    /* Rust is given the visible samples of each plane and nothing else:
     * rows - 1 full rows plus one visible row. */
    if (width <= 0 || height <= 0 ||
        pic->linesize[0] < width || pic->linesize[1] < cw || pic->linesize[2] < cw)
        return AVERROR_BUG;

    if (avctx->flags & AV_CODEC_FLAG_GRAY)
        flags |= VBLE_RS_GRAY;
    if (s->sse2_overwrite)
        flags |= VBLE_RS_SSE2_OVERWRITE;

    ret = vble_rs_restore(s->dec, avpkt->data, avpkt->size,
                          pic->data[0], (size_t)pic->linesize[0] * (height - 1) + width,
                          pic->linesize[0],
                          pic->data[1], (size_t)pic->linesize[1] * (ch - 1) + cw,
                          pic->linesize[1],
                          pic->data[2], (size_t)pic->linesize[2] * (ch - 1) + cw,
                          pic->linesize[2],
                          width, height, flags);
    if (ret < 0)
        return AVERROR_BUG;

    *got_frame = 1;

    return avpkt->size;
}

static av_cold int vble_rs_decode_close(AVCodecContext *avctx)
{
    VbleRsContext *s = avctx->priv_data;

    vble_rs_free(s->dec);
    s->dec = NULL;

    return 0;
}

const FFCodec ff_vble_rs_decoder = {
    .p.name         = "vble_rs",
    CODEC_LONG_NAME("VBLE Lossless Codec, safe Rust port"),
    .p.type         = AVMEDIA_TYPE_VIDEO,
    .p.id           = AV_CODEC_ID_VBLE,
    .priv_data_size = sizeof(VbleRsContext),
    .init           = vble_rs_decode_init,
    .close          = vble_rs_decode_close,
    FF_CODEC_DECODE_CB(vble_rs_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DR1 | AV_CODEC_CAP_FRAME_THREADS,
    .caps_internal  = FF_CODEC_CAP_INIT_CLEANUP,
};
