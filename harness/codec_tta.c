/*
 * TTA (True Audio): packet generators.
 *
 * Most packets come from a small TTA encoder (a reimplementation of the
 * coding in FFmpeg's ttaenc.c) run on synthetic signals, so that frames
 * have exactly the right number of samples; the rest are streams of Rice
 * codes with arbitrary values, and random bytes. Everything here uses
 * unsigned arithmetic, because the sanitizer build checks this file too.
 */

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libavutil/common.h"
#include "libavutil/intreadwrite.h"
#include "libavutil/macros.h"
#include "libavutil/opt.h"

#include "common.h"

/* Test hook in the Rust library: 0 forces the portable decoder. */
void tta_rs_set_simd(int enabled);

/* ---- tables and state, as in ttadata.c --------------------------------- */

static const uint32_t shift_1[] = {
    0x00000001, 0x00000002, 0x00000004, 0x00000008,
    0x00000010, 0x00000020, 0x00000040, 0x00000080,
    0x00000100, 0x00000200, 0x00000400, 0x00000800,
    0x00001000, 0x00002000, 0x00004000, 0x00008000,
    0x00010000, 0x00020000, 0x00040000, 0x00080000,
    0x00100000, 0x00200000, 0x00400000, 0x00800000,
    0x01000000, 0x02000000, 0x04000000, 0x08000000,
    0x10000000, 0x20000000, 0x40000000, 0x80000000,
    0x80000000, 0x80000000, 0x80000000, 0x80000000,
    0x80000000, 0x80000000, 0x80000000, 0x80000000,
    0xFFFFFFFF
};
static const uint32_t *const shift_16 = shift_1 + 4;
static const uint8_t filter_configs[] = { 10, 9, 10, 12 };

typedef struct Rice {
    uint32_t k0, k1, sum0, sum1;
} Rice;

typedef struct Filt {
    int shift;
    uint32_t round;
    int32_t error;
    uint32_t qm[8];
    int32_t dx[8];
    uint32_t dl[8];
} Filt;

typedef struct Chan {
    int32_t predictor;
    Filt f;
    Rice r;
} Chan;

#define MAX_CH 16
#define MAX_SAMPLES (1 << 20)

static Chan chan[MAX_CH];
static int32_t smp[MAX_SAMPLES];

/* ---- bit writer (least significant bit first) -------------------------- */

typedef struct BW {
    Buf *b;
    uint64_t acc;
    int n;
} BW;

static void bw_put(BW *w, int nbits, uint32_t v)
{
    if (nbits < 32)
        v &= (1u << nbits) - 1;
    w->acc |= (uint64_t)v << w->n;
    w->n   += nbits;
    while (w->n >= 8) {
        put(w->b, w->acc & 0xff);
        w->acc >>= 8;
        w->n    -= 8;
    }
}

static void bw_flush(BW *w)
{
    if (w->n)
        put(w->b, w->acc & 0xff);
    w->acc = 0;
    w->n   = 0;
}

/* ---- checksums --------------------------------------------------------- */

static uint32_t crc32_ieee(const uint8_t *d, int n)
{
    static uint32_t table[256];
    uint32_t crc = 0xFFFFFFFFu;

    if (!table[1])
        for (uint32_t i = 0; i < 256; i++) {
            uint32_t c = i;
            for (int k = 0; k < 8; k++)
                c = c & 1 ? 0xEDB88320u ^ (c >> 1) : c >> 1;
            table[i] = c;
        }
    for (int i = 0; i < n; i++)
        crc = table[(crc ^ d[i]) & 0xff] ^ (crc >> 8);
    return crc ^ 0xFFFFFFFFu;
}

static void put_crc(Buf *b)
{
    uint32_t crc = crc32_ieee(b->d, b->n);
    for (int i = 0; i < 4; i++)
        put(b, crc >> (8 * i));
}

static uint64_t crc64_password(const char *pass)
{
    uint64_t crc = UINT64_MAX, poly = 0x42F0E1EBA9EA3693U;

    for (; *pass; pass++) {
        crc ^= (uint64_t)(uint8_t)*pass << 56;
        for (int i = 0; i < 8; i++)
            crc = (crc << 1) ^ (crc >> 63 ? poly : 0);
    }
    return crc ^ UINT64_MAX;
}

/* ---- decoder options --------------------------------------------------- */

/* opt[0]: error recognition; opt[1]: password. */
static const char *const passwords[] = { NULL, "ffmpeg", "wrong", "" };

static const char *password_of(const Cfg *cfg)
{
    return passwords[cfg->opt[1] & 3];
}

static int use_simd = 1;

static void options(Cfg *cfg)
{
    static const uint8_t pw[8] = { 0, 1, 1, 1, 1, 2, 2, 3 };

    cfg->opt[0] = rnd(2) ? 0 : 1 + rnd(3);
    cfg->opt[1] = pw[rnd(8)];
    /* A quarter of the cases run the Rust decoder without its SSE filter. */
    use_simd = rnd(4) != 0;
    tta_rs_set_simd(use_simd);
}

static void setup(AVCodecContext *ctx, const Cfg *cfg)
{
    switch (cfg->opt[0]) {
    case 1: ctx->err_recognition = AV_EF_CRCCHECK;                 break;
    case 2: ctx->err_recognition = AV_EF_CRCCHECK | AV_EF_EXPLODE; break;
    case 3: ctx->err_recognition = AV_EF_EXPLODE;                  break;
    }
    if (password_of(cfg) && ctx->priv_data)
        av_opt_set(ctx->priv_data, "password", password_of(cfg), 0);
    /* For benchmarking the portable decoder. */
    if (getenv("TTA_RS_PORTABLE"))
        tta_rs_set_simd(0);
    /* For reproducing a mismatch: the configuration of every case. */
    if (getenv("TTA_DEBUG")) {
        fprintf(stderr, "cfg simd=%d opt=%d,%d extradata=", use_simd, cfg->opt[0], cfg->opt[1]);
        for (int i = 0; i < cfg->extradata_size; i++)
            fprintf(stderr, "%02x", cfg->extradata[i]);
        fputc('\n', stderr);
    }
}

/* Repair the frame CRC of most mutated packets when it is being checked. */
static void fixup(Buf *b, const Cfg *cfg)
{
    if ((cfg->opt[0] == 1 || cfg->opt[0] == 2) && b->n >= 4 && rnd(4)) {
        b->n -= 4;
        put_crc(b);
    }
}

/* ---- stream header ----------------------------------------------------- */

typedef struct Hdr {
    int format, channels, bits, bps;
    uint32_t rate, data_length;
    int frame_length, last;
} Hdr;

static void write_header(Cfg *cfg, int format, int channels, int bits,
                         uint32_t rate, uint32_t data_length)
{
    uint8_t *h = cfg->extradata;

    memcpy(h, "TTA1", 4);
    AV_WL16(h + 4, format);
    AV_WL16(h + 6, channels);
    AV_WL16(h + 8, bits);
    AV_WL32(h + 10, rate);
    AV_WL32(h + 14, data_length);
    AV_WL32(h + 18, crc32_ieee(h, 18));
    cfg->extradata_size = 22;
    cfg->bits           = bits;
}

/* The header of a configuration the decoder accepted. */
static void read_header(const Cfg *cfg, Hdr *h)
{
    const uint8_t *e = cfg->extradata;

    h->format      = AV_RL16(e + 4);
    h->channels    = av_clip(AV_RL16(e + 6), 1, MAX_CH);
    h->bits        = AV_RL16(e + 8);
    h->bps         = av_clip((h->bits + 7) / 8, 1, 3);
    h->rate        = AV_RL32(e + 10) & 0x7FFFFF;
    h->data_length = AV_RL32(e + 14);
    h->frame_length = 256 * h->rate / 245;
    if (h->frame_length < 1)
        h->frame_length = 1;
    h->last = h->data_length % h->frame_length;
}

static void config(Cfg *cfg)
{
    static const int common_bits[] = { 8, 16, 24 };
    int channels, bits, format = 1;
    uint32_t rate, data_length, frame_length;

    switch (rnd(8)) {
    case 0: case 1:         channels = 1;            break;
    case 2: case 3: case 4: channels = 2;            break;
    case 5: case 6:         channels = 3 + rnd(6);   break;
    default:                channels = 9 + rnd(8);   break;
    }
    if (rnd(50) == 0)
        channels = rnd(2) ? 0 : 17 + rnd(3);

    bits = rnd(5) ? common_bits[rnd(3)] : 1 + rnd(24);
    if (rnd(50) == 0)
        bits = rnd(2) ? 0 : 25 + rnd(8);

    /* Frames are 256 * rate / 245 samples long; keep them small. */
    switch (rnd(8)) {
    case 0:  rate = 1 + rnd(8);    break;
    case 1:  rate = 1 + rnd(3000); break;
    default: rate = 1 + rnd(400);  break;
    }
    if (rnd(60) == 0) {
        static const uint32_t bad[] = { 0, 0x800000, 0x80000000u, 0xFFFFFFFFu };
        rate = bad[rnd(4)];
    }
    frame_length = FFMAX(256 * (uint64_t)rate / 245, 1);

    switch (rnd(4)) {
    case 0:  data_length = 0;                           break;
    case 1:  data_length = rnd64();                     break;
    default: data_length = rnd(3 * frame_length + 1);   break;
    }

    if (rnd(6) == 0)
        format = 2;                     /* encrypted */
    else if (rnd(30) == 0)
        format = 0;
    else if (rnd(60) == 0)
        format = 3 + rnd(4);

    write_header(cfg, format, channels, bits, rate, data_length);
    switch (rnd(60)) {
    case 0:                             /* longer than the header */
        for (int i = 1 + rnd(12); i > 0; i--)
            cfg->extradata[cfg->extradata_size++] = rnd(256);
        break;
    case 1:                             /* cut short */
        cfg->extradata_size = rnd(22);
        break;
    case 2:                             /* not a TTA1 header */
        cfg->extradata[rnd(4)] ^= 1 << rnd(8);
        break;
    }

    /* What a container would say; the decoder goes by the header. */
    cfg->sample_rate = rnd(2) ? (int)(rate & 0x7FFFFF) : 1 + (int)rnd(96000);
    cfg->channels    = rnd(3) ? av_clip(channels, 1, MAX_CH) : (int)rnd(9);
}

/* ---- encoder ----------------------------------------------------------- */

static void enc_init(const Hdr *h, const char *pass)
{
    uint64_t crc = pass && h->format == 2 ? crc64_password(pass) : 0;

    for (int i = 0; i < h->channels; i++) {
        Chan *c = &chan[i];
        memset(c, 0, sizeof(*c));
        c->f.shift = filter_configs[h->bps - 1];
        c->f.round = shift_1[c->f.shift - 1];
        if (pass && h->format == 2)
            for (int k = 0; k < 8; k++)
                c->f.qm[k] = (uint32_t)(int8_t)(crc >> (8 * k));
        c->r.k0 = c->r.k1 = 10;
        c->r.sum0 = c->r.sum1 = shift_16[10];
    }
}

static void rice_adapt(uint32_t *k, uint32_t *sum, uint32_t value)
{
    *sum += value - (*sum >> 4);
    if (*k > 0 && *sum < shift_16[*k])
        (*k)--;
    else if (*sum > shift_16[*k + 1])
        (*k)++;
}

/* One Rice code. Runs of ones are cut at max_run, which only happens for
 * values no real encoder would be given. */
static void put_code(BW *w, Rice *r, uint32_t outval, uint32_t max_run)
{
    uint32_t k = r->k0;

    rice_adapt(&r->k0, &r->sum0, outval);
    if (outval >= shift_1[k]) {
        uint32_t unary;
        outval -= shift_1[k];
        k = r->k1;
        rice_adapt(&r->k1, &r->sum1, outval);
        unary = k < 32 ? 1 + (outval >> k) : 1;
        if (unary > max_run)
            unary = max_run;
        for (; unary > 31; unary -= 31)
            bw_put(w, 31, 0x7FFFFFFF);
        bw_put(w, unary, (1u << unary) - 1);
    }
    bw_put(w, 1, 0);
    if (k)
        bw_put(w, FFMIN(k, 32), outval);
}

static int32_t pred(int32_t x, int k)
{
    return (int32_t)((((uint64_t)x << k) - x) >> k);
}

/* ttaenc_filter_process_c, in unsigned arithmetic */
static uint32_t enc_filter(Filt *f, uint32_t in)
{
    uint32_t sum = f->round;

    for (int i = 0; i < 8; i++) {
        if (f->error < 0)
            f->qm[i] -= f->dx[i];
        else if (f->error > 0)
            f->qm[i] += f->dx[i];
        sum += f->dl[i] * f->qm[i];
    }
    for (int i = 0; i < 4; i++) {
        f->dx[i] = f->dx[i + 1];
        f->dl[i] = f->dl[i + 1];
    }
    f->dx[4] = ((int32_t)f->dl[4] >> 30) | 1;
    f->dx[5] = (((int32_t)f->dl[5] >> 30) | 2) & ~1;
    f->dx[6] = (((int32_t)f->dl[6] >> 30) | 2) & ~1;
    f->dx[7] = (((int32_t)f->dl[7] >> 30) | 4) & ~3;

    f->dl[4]  = -f->dl[5];
    f->dl[5]  = -f->dl[6];
    f->dl[6]  = in - f->dl[7];
    f->dl[7]  = in;
    f->dl[5] += f->dl[6];
    f->dl[4] += f->dl[5];

    in -= (uint32_t)((int32_t)sum >> f->shift);
    f->error = (int32_t)in;
    return in;
}

/* Encode n samples per channel from smp[] and append the CRC. */
static void encode(Buf *b, const Hdr *h, int n, const char *pass)
{
    BW w = { b };
    int nch = h->channels, total = n * nch;
    int32_t res = 0;

    enc_init(h, pass);
    for (int i = 0; i < total; i++) {
        int cur = i % nch;
        Chan *c = &chan[cur];
        uint32_t value = smp[i], temp, outval;

        if (nch > 1) {
            if (cur < nch - 1)
                value = res = (int32_t)((uint32_t)smp[i + 1] - value);
            else
                value -= res / 2;
        }
        temp   = value;
        value -= pred(c->predictor, h->bps == 1 ? 4 : 5);
        c->predictor = temp;
        value  = enc_filter(&c->f, value);
        outval = (int32_t)value > 0 ? (value << 1) - 1 : (0u - value) << 1;
        put_code(&w, &c->r, outval, 1 << 16);
    }
    bw_flush(&w);
    put_crc(b);
}

/* ---- signals ----------------------------------------------------------- */

static double phase;

/* Fill smp[] with n samples per channel of a synthetic signal. */
static void make_signal(const Hdr *h, int n, int kind)
{
    int nch = h->channels, total = n * nch;
    int vbits = 8 * h->bps;
    int32_t lim = (1 << (vbits - 1)) - 1;
    int amp = 1 + rnd(lim), period = 2 + rnd(400), nb = rnd(vbits);

    for (int i = 0; i < total; i++) {
        int t = i / nch, ch = i % nch;
        int64_t v;
        switch (kind) {
        case 0:                         /* silence */
            v = 0;
            break;
        case 1:                         /* a constant */
            v = amp - lim / 2;
            break;
        case 2: {                       /* triangle wave plus noise */
            int ph = (t + 7 * ch) % (2 * period);
            v = (int64_t)amp * (ph < period ? ph : 2 * period - ph) / period - amp / 2;
            v += (int64_t)(rnd64() & ((1 << nb) - 1)) - (1 << nb) / 2;
            break;
        }
        case 3:                         /* white noise, full scale */
            v = (int64_t)(rnd64() & ((1u << vbits) - 1)) - lim - 1;
            break;
        case 4:                         /* square wave between the extremes */
            v = (t / period + ch) & 1 ? lim : -lim - 1;
            break;
        case 5:                         /* sparse impulses */
            v = rnd(period) == 0 ? (rnd(2) ? amp : -amp) : 0;
            break;
        case 6:                         /* tones, for the benchmark */
            v = lim * (0.35 * sin(phase + 0.0626 * t + ch) + 0.2 * sin(0.4 * phase + 0.398 * t + 2 * ch)) +
                (int)(rnd64() & 7) - 4;
            break;
        default:                        /* any 32-bit value */
            v = (int32_t)rnd64();
            break;
        }
        if (kind <= 6)
            v = av_clip64(v, -lim - 1, lim);
        smp[i] = v;
    }
    phase += 0.0626 * n;
}

/* ---- fuzzing ----------------------------------------------------------- */

static void gen(Buf *b, const Cfg *cfg)
{
    static const uint8_t kinds[] = { 0, 1, 2, 2, 2, 2, 3, 3, 4, 5, 7 };
    const char *pass = password_of(cfg);
    Hdr h;
    int n, tail = 1;

    read_header(cfg, &h);
    b->n = 0;
    n    = h.frame_length;
    if (h.channels * (int64_t)n > MAX_SAMPLES)
        n = MAX_SAMPLES / h.channels;

    switch (rnd(16)) {
    case 9:                             /* the stream's last frame */
        if (h.last)
            n = h.last;
        break;
    case 10:                            /* a wrong number of samples */
        n = rnd(n + 4);
        break;
    case 11: case 12: {                 /* Rice codes with arbitrary values */
        BW w = { b };
        /* Half of these packets have runs of ones about as long as a
         * 64-bit bit reader's window (the port has one): in the last one
         * or two codes, so that the run is in the last bytes of the
         * packet, and in about four others. Each such code makes the
         * second Rice parameter grow by one and little brings it down
         * again, so there cannot be many. */
        int window_runs = rnd(2), total = n * h.channels, at_end = 1 + rnd(2);
        int maxbits = rnd(window_runs ? 10 : 32), run = 1 + rnd(rnd(4) ? 40 : 3000);
        enc_init(&h, pass);
        for (int i = 0; i < total; i++) {
            Rice *r  = &chan[i % h.channels].r;
            int bits = rnd(maxbits + 1);
            if (window_runs && r->k0 <= 20 && r->k1 <= 20 &&
                (i >= total - at_end || rnd(total) < 4)) {
                uint32_t ones = rnd(3) ? 56 + rnd(10) : 40 + rnd(90);
                put_code(&w, r, shift_1[r->k0] + ((ones - 1) << r->k1) +
                                ((uint32_t)rnd64() & (shift_1[r->k1] - 1)), 1 << 16);
            } else
                put_code(&w, r, bits ? (uint32_t)rnd64() >> (32 - bits) : 0, run);
        }
        bw_flush(&w);
        put_crc(b);
        tail = 0;
        break;
    }
    case 13: {                          /* random bytes, mostly ones or zeros */
        int len = rnd(4) ? rnd(4 * n * h.channels + 16) : rnd(64), dense = rnd(3);
        for (int i = 0; i < len; i++)
            put(b, dense == 0 ? rnd(256) : dense == 1 ? rnd(256) & rnd(256) : rnd(256) | rnd(256));
        return;
    }
    case 14: {                          /* long runs of ones */
        int len = rnd(2000);
        for (int i = 0; i < len; i++)
            put(b, rnd(20) ? 0xff : rnd(256));
        if (rnd(2))
            put_crc(b);
        return;
    }
    }
    if (tail) {
        make_signal(&h, n, kinds[rnd(sizeof(kinds))]);
        encode(b, &h, n, pass);
    }

    switch (rnd(24)) {
    case 0:                             /* no CRC */
        b->n = FFMAX(b->n - 4, 0);
        break;
    case 1:                             /* cut inside the CRC */
        b->n = FFMAX(b->n - 1 - (int)rnd(3), 0);
        break;
    case 2:                             /* bytes after the CRC */
        for (int i = 1 + rnd(6); i > 0; i--)
            put(b, rnd(256));
        break;
    case 3:                             /* a wrong CRC */
        if (b->n)
            b->d[b->n - 1 - rnd(FFMIN(b->n, 4))] ^= 1 << rnd(8);
        break;
    }
}

/* ---- benchmark --------------------------------------------------------- */

/* One full frame of a single kind of signal; cfg->w is the sample rate. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static const struct {
        const char *name;
        int bits, channels, signal, format;
    } kinds[] = {
        { "s16-silence",  16, 2, 0, 1 },
        { "s16-tones",    16, 2, 6, 1 },
        { "s16-noise",    16, 2, 3, 1 },
        { "u8-tones",      8, 2, 6, 1 },
        { "u8-noise-1ch",  8, 1, 3, 1 },
        { "s24-tones",    24, 2, 6, 1 },
        { "s24-noise",    24, 2, 3, 1 },
        { "s16-tones-1ch", 16, 1, 6, 1 },
        { "s16-tones-6ch", 16, 6, 6, 1 },
        { "s16-encrypted", 16, 2, 6, 2 },
    };
    static Buf b;
    AVPacket *pkt;
    Hdr h;

    for (size_t i = 0; i < FF_ARRAY_ELEMS(kinds); i++) {
        if (strcmp(kind, kinds[i].name))
            continue;
        write_header(cfg, kinds[i].format, kinds[i].channels, kinds[i].bits, cfg->w, 0);
        cfg->sample_rate = cfg->w;
        cfg->channels    = kinds[i].channels;
        cfg->opt[1]      = kinds[i].format == 2;        /* the password */
        read_header(cfg, &h);
        if ((int64_t)h.frame_length * h.channels > MAX_SAMPLES)
            return NULL;
        b.n = 0;
        make_signal(&h, h.frame_length, kinds[i].signal);
        encode(&b, &h, h.frame_length, password_of(cfg));
        pkt = av_packet_alloc();
        av_new_packet(pkt, b.n);
        memcpy(pkt->data, b.d, b.n);
        return pkt;
    }
    return NULL;
}

static const uint8_t interesting[] = { 0x00, 0xff, 0x7f, 0x80, 0x01, 0xfe, 0x55, 0xaa };

const CodecOps ops_tta = {
    .name           = "tta",
    .c_name         = "tta",
    .rs_name        = "tta_rs",
    .id             = AV_CODEC_ID_TTA,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .options        = options,
    .setup          = setup,
    .fixup          = fixup,
    .synth_kinds    = "s16-silence s16-tones s16-noise u8-tones u8-noise-1ch s24-tones s24-noise s16-tones-1ch s16-tones-6ch s16-encrypted",
    .synth          = synth,
};
