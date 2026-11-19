/*
 * FFmpeg's AIC decoder (libavcodec/aic.c, tag n8.1.3) keeps coefficients
 * from one slice to the next when a slice is narrower than the full slice
 * width, so the same packet decodes to different pictures depending on
 * what was decoded before it.
 *
 * aic_decode_init() places the four coefficient bands in one buffer at
 * offsets of ctx->slice_width * { 0, 64, 96, 288 }. aic_decode_slice()
 * clears only the first slice_width * 384 values of that buffer, where
 * slice_width is the width of the current slice. For a slice narrower
 * than ctx->slice_width that leaves all or part of bands 1 to 3 as the
 * previous slice wrote them, and a band coded with zero runs does not
 * overwrite what it skips. This happens for every slice of a picture up
 * to 16 pixels wide, and for the last slice of each row where the width
 * in macroblocks has no divisor that gives slices of at most 32 (37, 41,
 * 43, 47, 53 macroblocks, ...: 848 pixels wide is one such size).
 *
 * This program decodes two 16x16 frames with the C decoder. The second
 * frame codes no coefficient at all, so it should be flat grey (128). The
 * first has one chroma coefficient. Output:
 *
 *   second frame alone:           U[0] = 128
 *   second frame after the first: U[0] = 148
 *
 * Build and run inside the container, against any FFmpeg build tree:
 *
 *   S=/build/w/aic/ffmpeg
 *   gcc -I$S /proj/results/aic-stale-coeffs-repro.c $S/libavcodec/libavcodec.a \
 *       $S/libswresample/libswresample.a $S/libavutil/libavutil.a \
 *       $S/ffsafe-lib/libffsafe_ffi.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -o /tmp/repro
 *   /tmp/repro          # the C decoder
 *   /tmp/repro aic_rs   # the port, which copies this
 */

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "libavcodec/avcodec.h"

/* Luma all skipped; chroma band: 20, then 31 zeros; the rest skipped. */
static const uint8_t first[44] = {
    0x01, 0x16, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x10, 0x00, 0x10, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x04, 0x00, 0x00, 0x00, 0x82, 0x80, 0x40, 0x80, 0x53, 0xff, 0xff, 0xff,
    0xfe, 0x0a, 0x01, 0x04, 0x14, 0x02, 0x00, 0x00,
};
/* Every coefficient of all four bands skipped. */
static const uint8_t second[40] = {
    0x01, 0x16, 0x00, 0x00, 0x00, 0x28, 0x00, 0x10, 0x00, 0x10, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x03, 0x00, 0x00, 0x00, 0x82, 0x80, 0x41, 0x05, 0x00, 0x82, 0x0a, 0x01,
    0x04, 0x14, 0x02, 0x00,
};

static int decode(AVCodecContext *ctx, const uint8_t *data, int size)
{
    AVPacket *pkt  = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    int u = -1;

    av_new_packet(pkt, size);
    memcpy(pkt->data, data, size);
    if (avcodec_send_packet(ctx, pkt) >= 0 && avcodec_receive_frame(ctx, frame) >= 0)
        u = frame->data[1][0];
    av_frame_free(&frame);
    av_packet_free(&pkt);
    return u;
}

static AVCodecContext *open_decoder(const char *name)
{
    const AVCodec *codec = avcodec_find_decoder_by_name(name);
    AVCodecContext *ctx  = avcodec_alloc_context3(codec);

    ctx->width        = 16;
    ctx->height       = 16;
    ctx->thread_count = 1;
    if (avcodec_open2(ctx, codec, NULL) < 0)
        return NULL;
    return ctx;
}

int main(int argc, char **argv)
{
    const char *name = argc > 1 ? argv[1] : "aic";
    AVCodecContext *ctx;

    if (!(ctx = open_decoder(name)))
        return 1;
    printf("second frame alone:           U[0] = %d\n", decode(ctx, second, sizeof(second)));
    avcodec_free_context(&ctx);

    ctx = open_decoder(name);
    decode(ctx, first, sizeof(first));
    printf("second frame after the first: U[0] = %d\n", decode(ctx, second, sizeof(second)));
    avcodec_free_context(&ctx);
    return 0;
}
