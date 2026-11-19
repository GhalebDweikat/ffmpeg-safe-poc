/*
 * Differential fuzzer for the safe decoder ports.
 *
 * Feeds identical packet sequences to FFmpeg's C decoder and to the Rust
 * port of the same codec through the public libavcodec API and requires,
 * after every packet, the same return codes and the same frames (see
 * frame_diff in common.c for what is compared).
 *
 * Where a codec provides it, the Rust entry point is also called directly
 * on a buffer that is exactly as large as the visible image.
 *
 * usage: difffuzz CODEC SEED SECONDS [SEEDFILE]
 *   without SEEDFILE: generated streams, random sizes and configurations
 *   with SEEDFILE:    mutations of that file's real packets
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

#define MAX_SEQ 8
#define MAX_OUT 64              /* frames taken from one packet */

typedef struct Dec {
    AVCodecContext *ctx;
    AVFrame *frame;
} Dec;

/* Returns 0 if the decoder refuses the configuration. */
static int open_dec(Dec *d, const CodecOps *ops, const char *name, const Cfg *cfg,
                    const AVCodecParameters *par)
{
    const AVCodec *codec = avcodec_find_decoder_by_name(name);
    if (!codec) {
        fprintf(stderr, "decoder %s not found\n", name);
        exit(3);
    }
    d->ctx = avcodec_alloc_context3(codec);
    if (par)
        avcodec_parameters_to_context(d->ctx, par);
    else
        cfg_to_context(cfg, d->ctx);
    d->ctx->thread_count = 1;
    if (ops->setup)
        ops->setup(d->ctx, cfg);
    d->frame = av_frame_alloc();
    return avcodec_open2(d->ctx, codec, NULL) >= 0;
}

static void close_dec(Dec *d)
{
    av_frame_free(&d->frame);
    avcodec_free_context(&d->ctx);
}

/* The stream of a file that holds this codec, or -1. */
int find_stream(AVFormatContext *fmt, enum AVCodecID id);
int find_stream(AVFormatContext *fmt, enum AVCodecID id)
{
    for (unsigned i = 0; i < fmt->nb_streams; i++)
        if (fmt->streams[i]->codecpar->codec_id == id)
            return i;
    return -1;
}

static const int sizes[][2] = {
    { 1, 1 },   { 2, 3 },    { 3, 2 },   { 4, 4 },    { 5, 5 },   { 7, 9 },
    { 8, 8 },   { 12, 4 },   { 16, 16 }, { 17, 13 },  { 32, 24 }, { 33, 31 },
    { 64, 48 }, { 100, 75 }, { 3, 64 },  { 64, 3 },   { 250, 4 }, { 4, 250 },
    { 160, 120 }, { 129, 2 },
    /* Rows that exactly fill a 32- or 64-byte aligned stride. */
    { 64, 7 },  { 128, 9 },  { 32, 6 },  { 63, 5 },   { 65, 6 },  { 96, 10 },
    { 48, 5 },  { 6, 6 },    { 12, 7 },  { 24, 3 },
};

int main(int argc, char **argv)
{
    const CodecOps *ops;
    AVPacket **seeds = NULL;
    AVCodecParameters *par = NULL;
    int nb_seeds = 0;
    uint64_t cases = 0, packets = 0, frames = 0, rejected = 0, refused = 0;
    uint64_t status_count[32] = { 0 };
    static Buf buf;
    AVPacket *pkt;
    double seconds;
    time_t start;

    if (argc < 4 || !(ops = find_codec(argv[1]))) {
        fprintf(stderr, "usage: %s CODEC SEED SECONDS [SEEDFILE]\n", argv[0]);
        return 3;
    }
    rng     = strtoull(argv[2], NULL, 0) * 0x9E3779B97F4A7C15ULL + 1;
    seconds = atof(argv[3]);
    av_log_set_level(AV_LOG_QUIET);

    if (argc > 4) {
        AVFormatContext *fmt = NULL;
        int vs;
        if (avformat_open_input(&fmt, argv[4], NULL, NULL) < 0 ||
            avformat_find_stream_info(fmt, NULL) < 0 ||
            (vs = find_stream(fmt, ops->id)) < 0) {
            fprintf(stderr, "cannot read %s\n", argv[4]);
            return 3;
        }
        par = avcodec_parameters_alloc();
        avcodec_parameters_copy(par, fmt->streams[vs]->codecpar);
        pkt = av_packet_alloc();
        while (av_read_frame(fmt, pkt) >= 0) {
            if (pkt->stream_index == vs && pkt->size > 0 && pkt->size <= MAX_PKT) {
                seeds = realloc(seeds, (nb_seeds + 1) * sizeof(*seeds));
                seeds[nb_seeds++] = av_packet_clone(pkt);
            }
            av_packet_unref(pkt);
        }
        av_packet_free(&pkt);
        avformat_close_input(&fmt);
        if (!nb_seeds) {
            fprintf(stderr, "no usable packets in %s\n", argv[4]);
            return 3;
        }
    }

    pkt   = av_packet_alloc();
    start = time(NULL);

    for (;;) {
        Dec c, rs;
        Cfg cfg = { 0 };
        int nseq, first = 0, c_open, rs_open;
        void *direct = NULL;

        if ((cases & 63) == 0 && difftime(time(NULL), start) >= seconds)
            break;
        cases++;

        if (nb_seeds) {
            first = rnd(nb_seeds);
            cfg.w = par->width;
            cfg.h = par->height;
        } else {
            if (rnd(3)) {
                int i = rnd(sizeof(sizes) / sizeof(sizes[0]));
                cfg.w = sizes[i][0];
                cfg.h = sizes[i][1];
            } else {
                cfg.w = 1 + rnd(80);
                cfg.h = 1 + rnd(80);
            }
            ops->config(&cfg);
        }
        if (ops->options)
            ops->options(&cfg);
        nseq = 1 + rnd(MAX_SEQ);

        c_open  = open_dec(&c,  ops, ops->c_name,  &cfg, par);
        rs_open = open_dec(&rs, ops, ops->rs_name, &cfg, par);
        if (c_open != rs_open) {
            printf("MISMATCH seed=%s case=%llu open: c=%d rs=%d size=%dx%d bits=%d\n",
                   argv[2], (unsigned long long)cases, c_open, rs_open,
                   cfg.w, cfg.h, cfg.bits);
            return 2;
        }
        if (!c_open) {
            refused++;
            close_dec(&c);
            close_dec(&rs);
            continue;
        }
        if (!nb_seeds && ops->direct_open)
            direct = ops->direct_open(&cfg);

        for (int i = 0; i < nseq; i++) {
            int cs, cr = 0, rss, rsr = 0, flags = 0, got = 0;
            const char *diff = NULL;

            if (nb_seeds) {
                const AVPacket *s = seeds[(first + i) % nb_seeds];
                memcpy(buf.d, s->data, s->size);
                buf.n = s->size;
                flags = s->flags & AV_PKT_FLAG_KEY;
                if (rnd(4)) {
                    mutate(&buf, ops);
                    if (ops->fixup)
                        ops->fixup(&buf, &cfg);
                }
            } else {
                ops->gen(&buf, &cfg);
                if (rnd(5) < 3) {
                    mutate(&buf, ops);
                    if (ops->fixup)
                        ops->fixup(&buf, &cfg);
                }
            }
            if (rnd(16) == 0)
                flags ^= AV_PKT_FLAG_KEY;
            if (buf.n < 1) {
                buf.d[0] = 0;
                buf.n    = 1;
            }

            av_packet_unref(pkt);
            if (av_new_packet(pkt, buf.n) < 0)
                return 3;
            memcpy(pkt->data, buf.d, buf.n);
            pkt->flags = flags;
            if (ops->palette_side_data && rnd(6) == 0) {
                uint8_t *pal = av_packet_new_side_data(pkt, AV_PKT_DATA_PALETTE,
                                                       AVPALETTE_SIZE);
                for (int k = 0; pal && k < AVPALETTE_SIZE; k++)
                    pal[k] = rnd(256);
            }
            packets++;

            /* Now and then drop the decoders' state the way a seek does. */
            if (rnd(40) == 0) {
                avcodec_flush_buffers(c.ctx);
                avcodec_flush_buffers(rs.ctx);
            }

            /* A packet can hold several frames (audio); take them all. */
            cs  = avcodec_send_packet(c.ctx, pkt);
            rss = avcodec_send_packet(rs.ctx, pkt);
            if (cs != rss)
                diff = "return codes";
            for (int k = 0; !diff && k < MAX_OUT; k++) {
                av_frame_unref(c.frame);
                av_frame_unref(rs.frame);
                cr  = avcodec_receive_frame(c.ctx, c.frame);
                rsr = avcodec_receive_frame(rs.ctx, rs.frame);
                if (cr != rsr)
                    diff = "return codes";
                else if (cr == 0)
                    diff = frame_diff(c.frame, rs.frame);
                if (cr || diff)
                    break;
                got++;
                if (direct)             /* codecs with a direct check give one frame */
                    break;
            }
            if (diff) {
                FILE *f = fopen("mismatch.bin", "wb");
                if (f) {
                    fwrite(buf.d, 1, buf.n, f);
                    fclose(f);
                }
                printf("MISMATCH seed=%s case=%llu packet=%d what=\"%s\" size=%dx%d "
                       "bits=%d c=(%d,%d) rs=(%d,%d) len=%d (saved mismatch.bin)\n",
                       argv[2], (unsigned long long)cases, i, diff, cfg.w, cfg.h,
                       cfg.bits, cs, cr, rss, rsr, buf.n);
                return 2;
            }

            if (direct) {
                char why[200] = "";
                int st = ops->direct_step(direct, &buf, rs.frame, got > 0,
                                          why, sizeof(why));
                if (st < 0 || st >= ops->nb_status) {
                    printf("DIRECT_MISMATCH seed=%s case=%llu size=%dx%d %s\n",
                           argv[2], (unsigned long long)cases, cfg.w, cfg.h, why);
                    return 2;
                }
                status_count[st]++;
            }

            frames += got;
            if (!got)
                rejected++;
        }

        if (direct)
            ops->direct_close(direct);
        close_dec(&c);
        close_dec(&rs);
    }

    av_packet_free(&pkt);
    for (int i = 0; i < nb_seeds; i++)
        av_packet_free(&seeds[i]);
    free(seeds);
    avcodec_parameters_free(&par);

    printf("RESULT codec=%s seed=%s mode=%s cases=%llu packets=%llu decoded=%llu "
           "rejected=%llu refused_configs=%llu",
           ops->name, argv[2], nb_seeds ? "mutate-real" : "generated",
           (unsigned long long)cases, (unsigned long long)packets,
           (unsigned long long)frames, (unsigned long long)rejected,
           (unsigned long long)refused);
    for (int i = 0; i < ops->nb_status; i++)
        printf(" %s=%llu", ops->status_names[i], (unsigned long long)status_count[i]);
    printf(" mismatches=0 panics=0\n");
    return 0;
}
