/*
 * Decode benchmark for the safe decoder ports.
 *
 * Loads every video packet of a file into memory, then times complete
 * decode passes through libavcodec for FFmpeg's C decoder and for the Rust
 * port, alternating between them. Before timing, it decodes the input once
 * with each and requires identical frames.
 *
 * usage: bench CODEC FILE [PASSES]
 *        bench CODEC synth:KIND:WxH:FRAMES [PASSES]
 *        bench CODEC kinds
 *
 * The synth form builds packets in memory that exercise one decoder path
 * each; "kinds" lists the paths a codec offers.
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "libavcodec/avcodec.h"
#include "libavformat/avformat.h"
#include "libavutil/log.h"

#include "common.h"

static AVPacket **pkts;
static int nb_pkts;
static AVCodecParameters *par;
static Cfg synth_cfg;

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static int cmp_double(const void *a, const void *b)
{
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

/* One full decode pass. Returns elapsed ms; optionally hashes every frame. */
static double pass(const char *name, uint64_t *hash, int *frames)
{
    const AVCodec *codec = avcodec_find_decoder_by_name(name);
    AVCodecContext *ctx;
    AVFrame *frame = av_frame_alloc();
    uint64_t h     = 0xcbf29ce484222325ULL;
    int n          = 0;
    double t0, t1;

    if (!codec) {
        fprintf(stderr, "decoder %s not found\n", name);
        exit(3);
    }
    ctx = avcodec_alloc_context3(codec);
    if (par)
        avcodec_parameters_to_context(ctx, par);
    else
        cfg_to_context(&synth_cfg, ctx);
    ctx->thread_count = 1;
    if (avcodec_open2(ctx, codec, NULL) < 0) {
        fprintf(stderr, "cannot open %s\n", name);
        exit(3);
    }

    t0 = now_ms();
    for (int i = 0; i < nb_pkts; i++) {
        if (avcodec_send_packet(ctx, pkts[i]) < 0)
            continue;
        while (avcodec_receive_frame(ctx, frame) == 0) {
            n++;
            if (hash)
                h = frame_hash(frame, h);
            av_frame_unref(frame);
        }
    }
    t1 = now_ms();

    if (hash)
        *hash = h;
    if (frames)
        *frames = n;
    av_frame_free(&frame);
    avcodec_free_context(&ctx);
    return t1 - t0;
}

int main(int argc, char **argv)
{
    const CodecOps *ops;
    const char *names[2];
    AVFormatContext *fmt = NULL;
    AVPacket *pkt;
    uint64_t hash[2], bytes = 0;
    int frames[2], vs, passes, w, h;
    double *t[2], med[2];

    if (argc < 3 || !(ops = find_codec(argv[1]))) {
        fprintf(stderr, "usage: %s CODEC FILE|synth:KIND:WxH:FRAMES|kinds [PASSES]\n", argv[0]);
        return 3;
    }
    if (!strcmp(argv[2], "kinds")) {
        printf("%s\n", ops->synth_kinds ? ops->synth_kinds : "");
        return 0;
    }
    names[0] = ops->c_name;
    names[1] = ops->rs_name;
    passes   = argc > 3 ? atoi(argv[3]) : 31;
    av_log_set_level(AV_LOG_QUIET);            /* log output would be timed too */

    if (!strncmp(argv[2], "synth:", 6)) {
        char kind[32];
        int n;
        if (sscanf(argv[2], "synth:%31[^:]:%dx%d:%d", kind, &w, &h, &n) != 4 || !ops->synth) {
            fprintf(stderr, "bad synth spec %s\n", argv[2]);
            return 3;
        }
        synth_cfg.w = w;
        synth_cfg.h = h;
        pkts = malloc(n * sizeof(*pkts));
        for (nb_pkts = 0; nb_pkts < n; nb_pkts++) {
            pkts[nb_pkts] = ops->synth(kind, &synth_cfg);
            if (!pkts[nb_pkts]) {
                fprintf(stderr, "unknown synth kind %s\n", kind);
                return 3;
            }
            bytes += pkts[nb_pkts]->size;
        }
    } else {
        if (avformat_open_input(&fmt, argv[2], NULL, NULL) < 0 ||
            avformat_find_stream_info(fmt, NULL) < 0 ||
            (vs = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0)) < 0) {
            fprintf(stderr, "cannot read %s\n", argv[2]);
            return 3;
        }
        par = avcodec_parameters_alloc();
        avcodec_parameters_copy(par, fmt->streams[vs]->codecpar);
        w   = par->width;
        h   = par->height;
        pkt = av_packet_alloc();
        while (av_read_frame(fmt, pkt) >= 0) {
            if (pkt->stream_index == vs) {
                pkts = realloc(pkts, (nb_pkts + 1) * sizeof(*pkts));
                pkts[nb_pkts++] = av_packet_clone(pkt);
                bytes += pkt->size;
            }
            av_packet_unref(pkt);
        }
        av_packet_free(&pkt);
        avformat_close_input(&fmt);
    }

    /* Correctness first: both decoders must produce the same pictures. */
    for (int d = 0; d < 2; d++)
        pass(names[d], &hash[d], &frames[d]);
    if (hash[0] != hash[1] || frames[0] != frames[1] || !frames[0]) {
        printf("DIFFERENT %s: %s %d frames %016llx, %s %d frames %016llx\n",
               argv[2], names[0], frames[0], (unsigned long long)hash[0],
               names[1], frames[1], (unsigned long long)hash[1]);
        return 2;
    }

    for (int d = 0; d < 2; d++)
        t[d] = malloc(passes * sizeof(double));
    for (int i = 0; i < 3; i++)                /* warm-up */
        for (int d = 0; d < 2; d++)
            pass(names[d], NULL, NULL);
    for (int i = 0; i < passes; i++)
        for (int k = 0; k < 2; k++) {
            int d = (i + k) & 1;               /* alternate which goes first */
            t[d][i] = pass(names[d], NULL, NULL);
        }
    for (int d = 0; d < 2; d++) {
        qsort(t[d], passes, sizeof(double), cmp_double);
        med[d] = t[d][passes / 2];
    }

    printf("BENCH file=%s size=%dx%d frames=%d packet_bytes=%llu identical=yes "
           "c_median_ms=%.3f c_min_ms=%.3f rs_median_ms=%.3f rs_min_ms=%.3f "
           "c_us_per_frame=%.2f rs_us_per_frame=%.2f rs_over_c_median=%.3f "
           "rs_over_c_min=%.3f\n",
           argv[2], w, h, frames[0],
           (unsigned long long)bytes, med[0], t[0][0], med[1], t[1][0],
           1e3 * med[0] / frames[0], 1e3 * med[1] / frames[1],
           med[1] / med[0], t[1][0] / t[0][0]);
    return 0;
}
