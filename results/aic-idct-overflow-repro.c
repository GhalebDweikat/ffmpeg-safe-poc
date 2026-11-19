/*
 * FFmpeg's AIC decoder (libavcodec/aic.c, tag n8.1.3) does not limit the
 * coefficients it hands to the IDCT, and FFmpeg's 8-bit "simple" IDCT
 * exists in two forms that treat overflow differently:
 *
 *   C (simple_idct_template.c):          the row pass stores its 32-bit
 *       results in int16_t, keeping the low 16 bits;
 *   x86-64 (x86/simple_idct10_template.asm, ff_simple_idct8_sse2/_avx):
 *       the row pass packs with saturation (packssdw), and the rounding
 *       term of the column pass is added to the first coefficient with a
 *       16-bit add (paddw [pw_32]) that wraps for values above 32735.
 *
 * So one packet decodes to two pictures, depending on how FFmpeg was built
 * and on the CPU flags. This program decodes one 16x16 frame with large
 * luma coefficients twice with the C decoder: with the CPU's extensions,
 * and with them turned off (the same as "ffmpeg -cpuflags 0"). Output on
 * an x86-64 build with assembly:
 *
 *   default CPU flags: Y hash cdca6700767e56c6, first pixels 0 0 0 255 255 0 0 0
 *   CPU flags off:     Y hash 3aa5417cf1174930, first pixels 43 255 255 0 255 0 0 255
 *
 * No encoder would write such coefficients; this matters only for damaged
 * or hostile input, where the two builds show different garbage.
 *
 * Build and run inside the container:
 *
 *   S=/build/w/aic/ffmpeg
 *   gcc -I$S /proj/results/aic-idct-overflow-repro.c $S/libavcodec/libavcodec.a \
 *       $S/libswresample/libswresample.a $S/libavutil/libavutil.a \
 *       $S/ffsafe-lib/libffsafe_ffi.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -o /tmp/repro
 *   /tmp/repro          # the C decoder
 *   /tmp/repro aic_rs   # the port, which follows whichever IDCT FFmpeg picks
 */

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "libavcodec/avcodec.h"
#include "libavutil/cpu.h"

/* Luma band: 64 coefficients, each an Exp-Golomb code plus 7 bits, values
 * spread over 0..65535; the other three bands skipped. Quantiser 16. */
static const uint8_t large[212] = {
    0x01, 0x16, 0x00, 0x00, 0x00, 0xd4, 0x00, 0x10, 0x00, 0x10, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x2e, 0x00, 0x00, 0x00, 0x7c, 0x00, 0x0d, 0xee, 0x60, 0x1b, 0xcc, 0xc0,
    0x26, 0xad, 0x00, 0x5e, 0x26, 0x81, 0x5a, 0x00, 0x04, 0xd1, 0xa0, 0x49,
    0x38, 0x0f, 0x03, 0x60, 0x1c, 0xe1, 0xc0, 0x2b, 0x00, 0x80, 0x62, 0x7a,
    0x81, 0x9f, 0x40, 0x05, 0x16, 0xd8, 0x23, 0x9e, 0x01, 0x01, 0x84, 0x01,
    0xdf, 0x6c, 0x02, 0xf5, 0x48, 0x06, 0x6c, 0xe0, 0x1e, 0x48, 0x00, 0x55,
    0xc1, 0x83, 0x4e, 0xe0, 0x11, 0x2d, 0x40, 0x1f, 0x0b, 0xa0, 0x33, 0xa8,
    0x80, 0x6b, 0x22, 0x00, 0x8a, 0x6e, 0x01, 0x68, 0x56, 0x04, 0x63, 0xc0,
    0x12, 0x42, 0x48, 0x28, 0x0d, 0xff, 0x00, 0x1b, 0xdd, 0x80, 0x26, 0xef,
    0x80, 0x5e, 0x69, 0x81, 0x5e, 0x30, 0x04, 0xd5, 0xc8, 0x4d, 0x68, 0x0f,
    0x14, 0x00, 0x1c, 0xf2, 0x60, 0x2b, 0x43, 0x80, 0x62, 0xbd, 0x01, 0xa3,
    0x70, 0x05, 0x1b, 0x08, 0x24, 0xa8, 0x01, 0x02, 0x90, 0x01, 0xe0, 0x76,
    0x02, 0xf9, 0x70, 0x06, 0x71, 0x10, 0x1e, 0x8a, 0x80, 0x56, 0x04, 0x83,
    0x5f, 0x80, 0x11, 0x3d, 0xe0, 0x1f, 0x1c, 0x60, 0x33, 0xeb, 0x00, 0x6b,
    0x65, 0x00, 0x8b, 0x7a, 0x01, 0x69, 0x60, 0x04, 0x74, 0x80, 0x12, 0x52,
    0xec, 0x50, 0x0e, 0x0f, 0xc0, 0x1b, 0xee, 0x20, 0x27, 0x32, 0xc1, 0x40,
    0x20, 0x82, 0x80, 0x41, 0x05, 0x00, 0x80, 0x00,
};

static void decode(const char *name, const char *label)
{
    const AVCodec *codec = avcodec_find_decoder_by_name(name);
    AVCodecContext *ctx  = avcodec_alloc_context3(codec);
    AVPacket *pkt        = av_packet_alloc();
    AVFrame *frame       = av_frame_alloc();
    uint64_t h           = 0xcbf29ce484222325ULL;

    ctx->width        = 16;
    ctx->height       = 16;
    ctx->thread_count = 1;
    av_new_packet(pkt, sizeof(large));
    memcpy(pkt->data, large, sizeof(large));
    if (avcodec_open2(ctx, codec, NULL) < 0 || avcodec_send_packet(ctx, pkt) < 0 ||
        avcodec_receive_frame(ctx, frame) < 0) {
        printf("%s: no frame\n", label);
    } else {
        for (int y = 0; y < 16; y++)
            for (int x = 0; x < 16; x++)
                h = (h ^ frame->data[0][y * frame->linesize[0] + x]) * 0x100000001b3ULL;
        printf("%s Y hash %016llx, first pixels", label, (unsigned long long)h);
        for (int x = 0; x < 8; x++)
            printf(" %d", frame->data[0][x]);
        printf("\n");
    }
    av_frame_free(&frame);
    av_packet_free(&pkt);
    avcodec_free_context(&ctx);
}

int main(int argc, char **argv)
{
    const char *name = argc > 1 ? argv[1] : "aic";

    decode(name, "default CPU flags:");
    av_force_cpu_flags(0);
    decode(name, "CPU flags off:    ");
    return 0;
}
