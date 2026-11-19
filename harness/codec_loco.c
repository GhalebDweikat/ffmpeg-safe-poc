/*
 * LOCO: packet generators.
 *
 * FFmpeg has no LOCO encoder, so this file has one: a Golomb-Rice writer
 * that follows the decoder's state machine (adaptive Rice parameter, zero
 * runs). The fuzzer uses it to write streams of random code values; the
 * benchmark uses it to encode generated pictures.
 */

#include <limits.h>
#include <math.h>
#include <stdlib.h>
#include <string.h>

#include "common.h"

/* ---- bit writer, most significant bit first ---- */

typedef struct BitW {
    Buf *b;
    int fill;                   /* bits used in the last byte; 0: none open */
} BitW;

static void bw_bits(BitW *w, uint32_t v, int n)
{
    while (n > 0) {
        int room, take;
        if (!w->fill) {
            if (w->b->n >= MAX_PKT)
                return;
            w->b->d[w->b->n++] = 0;
        }
        room = 8 - w->fill;
        take = n < room ? n : room;
        w->b->d[w->b->n - 1] |= ((v >> (n - take)) & ((1u << take) - 1)) << (room - take);
        w->fill = (w->fill + take) & 7;
        n -= take;
    }
}

static void bw_zeros(BitW *w, uint32_t n)
{
    while (n && w->fill) {
        bw_bits(w, 0, 1);
        n--;
    }
    while (n >= 8) {
        if (w->b->n >= MAX_PKT)
            return;
        w->b->d[w->b->n++] = 0;
        n -= 8;
    }
    bw_bits(w, 0, n);
}

/* End of a plane: the next one starts on a byte boundary. */
static void bw_align(BitW *w, int random_bits)
{
    if (w->fill)
        bw_bits(w, random_bits ? rnd(256) : 0, 8 - w->fill);
}

/* One code: z zero bits, a one, k more bits. Returns the value the decoder
 * computes from it, which wraps at 32 bits. */
static uint32_t put_code(BitW *w, uint32_t z, uint32_t low, int k)
{
    bw_zeros(w, z);
    bw_bits(w, 1, 1);
    if (k)
        bw_bits(w, low, k);
    return (z << k) + low;
}

/* ---- the decoder's Rice state (RICEContext in loco.c) ---- */

typedef struct RiceW {
    int32_t sum;
    int count, save, run2;
} RiceW;

static void rice_init(RiceW *r)
{
    r->sum   = 8;
    r->count = 1;
    r->save  = 0;
    r->run2  = 0;
}

static int rice_k(const RiceW *r)
{
    int cnt = 0, val = r->count;

    while (r->sum > val && cnt < 9) {
        val <<= 1;
        cnt++;
    }
    return cnt;
}

/* The C decoder adds every code value (halved) to a signed int. Adding
 * this one would overflow it, which is undefined behaviour in C. */
static int rice_overflows(const RiceW *r, uint32_t val)
{
    return (int64_t)r->sum + (int64_t)val > INT_MAX;
}

static void rice_update(RiceW *r, uint32_t val)
{
    r->sum = (int32_t)((uint32_t)r->sum + val);
    r->count++;
    if (r->count == 16) {
        r->sum   >>= 1;
        r->count >>= 1;
    }
}

static void rice_nonzero(RiceW *r)
{
    if (r->run2 > 0) {
        if (r->run2 > 2)
            r->save += r->run2;
        else
            r->save -= 3;
        r->run2 = 0;
    }
}

/* Set LOCO_UB=1 to let the generator write, and the fuzzer keep, packets
 * that overflow the C decoder's sum. */
static int allow_ub(void)
{
    static int v = -1;
    if (v < 0)
        v = getenv("LOCO_UB") != NULL;
    return v;
}

/* A code written with a chosen number of leading zeros instead of the
 * number its value needs. */
typedef struct Special {
    int at;                     /* pixel index, -1: none */
    uint32_t z;
} Special;

/* Encode n pixels. vals[i] is the code value of pixel i; 0 is a zero
 * residual. runs: 0 codes every zero run at its full length, 1 sometimes
 * cuts one short, 2 writes run lengths that have nothing to do with the
 * values (all of these are valid streams). */
static void enc_plane(BitW *w, const uint32_t *vals, int n, int runs, const Special *sp)
{
    RiceW r;

    rice_init(&r);
    for (int p = 0; p < n && w->b->n < MAX_PKT - 16;) {
        int k = rice_k(&r);
        uint32_t low = vals[p] & ((1u << k) - 1), z = vals[p] >> k, v;

        if (sp && sp->at == p)
            z = sp->z;
        v = (z << k) + low;
        while (!allow_ub() && v != UINT32_MAX && rice_overflows(&r, (v + 1) >> 1)) {
            z >>= 1;
            if (!z)
                low = 0;
            v = (z << k) + low;
        }
        put_code(w, z, low, k);
        p++;
        /* A value of all ones is the C reader's error value, and the
         * decoder stops there. The stream goes on as if it were a value
         * like any other, so that a decoder without that check would get
         * a whole picture out of it and be found out. */
        rice_update(&r, (v + 1) >> 1);
        if (!v) {
            if (r.save >= 0) {
                int run = 0;
                while (p + run < n && !vals[p + run])
                    run++;
                if (runs == 1 && run && !rnd(4))
                    run = rnd(run + 1);
                if (runs == 2)
                    run = rnd(4) ? rnd(8) : rnd(n + 2);
                put_code(w, run >> 2, run & 3, 2);
                if (run > 1)
                    r.save += run + 1;
                else
                    r.save -= 3;
                for (int q = 0; q < run; q++)
                    rice_update(&r, 0);
                p += run;
            } else {
                r.run2++;
            }
        } else {
            rice_nonzero(&r);
        }
    }
}

/* ---- stream parameters ---- */

static void wl32(uint8_t *p, uint32_t v)
{
    p[0] = v; p[1] = v >> 8; p[2] = v >> 16; p[3] = v >> 24;
}

static int32_t rl32(const uint8_t *p)
{
    return (int32_t)(p[0] | p[1] << 8 | p[2] << 16 | (uint32_t)p[3] << 24);
}

static void set_extradata(Cfg *cfg, int version, int mode, int lossy)
{
    wl32(cfg->extradata, version);
    wl32(cfg->extradata + 4, mode);
    wl32(cfg->extradata + 8, lossy);
    cfg->extradata_size = 12;
}

/* The coded planes of a stream, in the order they are in a packet. */
typedef struct Layout {
    int lossy;
    int nb_planes;
    int w[4], h[4];
} Layout;

/* Returns 0 for parameters the decoder refuses. */
static int layout(const Cfg *cfg, Layout *l)
{
    int version, mode, cw, ch;

    if (cfg->extradata_size < 12)
        return 0;
    version  = rl32(cfg->extradata);
    mode     = rl32(cfg->extradata + 4);
    l->lossy = version == 1 ? 0 : rl32(cfg->extradata + 8);
    if (l->lossy < 0 || l->lossy > 65536)
        return 0;
    /* A plane coded with width 0 still has one pixel per row, and one
     * coded with height 0 still has its first row. */
    cw = cfg->w / 2 ? cfg->w / 2 : 1;
    ch = cfg->h / 2 ? cfg->h / 2 : 1;
    for (int i = 0; i < 4; i++) {
        l->w[i] = cfg->w;
        l->h[i] = cfg->h;
    }
    l->nb_planes = 3;
    switch (mode) {
    case -1: case 1: case 2:            /* YUY2, UYVY: 4:2:2 */
        l->w[1] = l->w[2] = cw;
        break;
    case -4: case 5:                    /* YV12: 4:2:0 */
        l->w[1] = l->w[2] = cw;
        l->h[1] = l->h[2] = ch;
        break;
    case -2: case 3:                    /* RGB */
        break;
    case -3: case 4:                    /* RGBA */
        l->nb_planes = 4;
        break;
    default:
        return 0;
    }
    return 1;
}

static void config(Cfg *cfg)
{
    static const int modes[] = { -1, -2, -3, -4, 1, 2, 3, 4, 5 };
    int version = rnd(10) ? 1 + rnd(2) : (int)rnd(6) - 1;
    int mode    = rnd(30) ? modes[rnd(9)] : (int)rnd(16) - 7;
    int r       = rnd(16), lossy;

    if (r < 6)
        lossy = 0;
    else if (r < 11)
        lossy = 1 + rnd(4);
    else if (r < 13)
        lossy = rnd(300);
    else if (r == 13)
        lossy = 65536 - rnd(3);
    else if (r == 14)
        lossy = 65537 + rnd(3);         /* refused, unless the version is 1 */
    else
        lossy = (int)rnd64();
    set_extradata(cfg, version, mode, lossy);
    switch (rnd(40)) {
    case 0:                             /* too short: refused */
        cfg->extradata_size = rnd(12);
        break;
    case 1:                             /* longer than needed */
        cfg->extradata_size = 13 + rnd(20);
        for (int i = 12; i < cfg->extradata_size; i++)
            cfg->extradata[i] = rnd(256);
        break;
    }
}

/* ---- fuzzing ---- */

static void random_values(uint32_t *vals, int n)
{
    int dist = rnd(8), seg = 0, zero = 0;

    for (int i = 0; i < n; i++) {
        uint32_t v;
        switch (dist) {
        case 0:  v = 0;                                         break;
        case 1:  v = rnd(8) ? 0 : 1 + rnd(6);                   break;
        case 2:  v = rnd(4);                                    break;
        case 3:  v = rnd(1 << rnd(7));                          break;
        case 4:  v = rnd(512);                                  break;
        case 5:  v = rnd(64) ? rnd(3) : rnd(1 << rnd(15));      break;
        case 6:                         /* stretches of zeros and of non-zeros */
            if (!seg) {
                seg  = 1 + rnd(rnd(4) ? 6 : 40);
                zero = !zero;
            }
            seg--;
            v = zero ? 0 : 1 + rnd(40);
            break;
        default: v = 1 + rnd(1 << rnd(9));                      break;
        }
        vals[i] = v;
    }
}

static void gen(Buf *b, const Cfg *cfg)
{
    Layout l;
    BitW w = { b, 0 };
    int planes;

    b->n = 0;
    if (!layout(cfg, &l) || rnd(24) == 0) {
        int n = 1 + rnd(rnd(4) ? 64 : 2048);
        for (int i = 0; i < n; i++)
            put(b, rnd(256));
        return;
    }
    /* Now and then the packet ends with one of the earlier planes. */
    planes = rnd(20) ? l.nb_planes : (int)rnd(l.nb_planes + 1);
    for (int i = 0; i < planes; i++) {
        int n = l.w[i] * l.h[i];
        uint32_t *vals = malloc(n * sizeof(*vals));
        Special sp = { -1, 0 };

        random_values(vals, n);
        if (n >= 3 && rnd(1000) == 0) {
            /* A megabyte of zero bits, so that the code value wraps at 32
             * bits. The value before it makes the Rice parameter 9. */
            sp.at = 1 + rnd(n - 1 < 6 ? n - 1 : 6);
            vals[sp.at - 1] = 10000 + rnd(1000);
            switch (rnd(allow_ub() ? 5 : 4)) {
            case 0:
                sp.z = (1u << 23) + rnd(3);
                break;
            case 2:
                sp.z = (1u << 23) + rnd(1 << rnd(8));
                break;
            case 1: case 3:             /* reads as -1, the reader's error value */
                sp.z = (1u << 23) - 1;
                vals[sp.at] = 0x1ff;
                break;
            case 4:                     /* overflows the C decoder's sum */
                sp.z = (1u << 23) - 1 - rnd(1 << rnd(22));
                break;
            }
        }
        enc_plane(&w, vals, n, rnd(3), sp.at >= 0 ? &sp : NULL);
        bw_align(&w, rnd(2));
        free(vals);
    }
    if (rnd(5) == 0)
        for (int n = 1 + rnd(8); n > 0; n--)
            put(b, rnd(256));
    if (!b->n)
        put(b, rnd(256));
}

/* ---- the C decoder's arithmetic, to find the packets that overflow it ---- */

typedef struct GetB {
    const uint8_t *d;
    uint32_t bytes, index, size, size_plus8;
} GetB;

static uint32_t gb_cache(const GetB *g, uint32_t index)
{
    uint32_t at = index >> 3, v = 0;

    for (uint32_t i = 0; i < 4; i++)
        v = v << 8 | (at + i < g->bytes ? g->d[at + i] : 0);
    return v << (index & 7);
}

static uint32_t gb_skip(const GetB *g, uint32_t index, uint32_t n)
{
    return index + n < g->size_plus8 ? index + n : g->size_plus8;
}

/* get_ur_golomb_jpegls(gb, k, INT_MAX, 0) */
static uint32_t gb_golomb(GetB *g, int k)
{
    uint32_t index = g->index, cache = gb_cache(g, index), i = 0, buf = 0;
    int log = 31 - __builtin_clz(cache | 1);

    if (log - k >= 7) {
        buf      = (cache >> (log - k)) + ((30u - log) << k);
        g->index = gb_skip(g, index, 32 + k - log);
        return buf;
    }
    while (!(cache >> 7)) {
        if (g->size <= index) {
            g->index = index;
            return UINT32_MAX;
        }
        index = gb_skip(g, index, 25);
        cache = gb_cache(g, index);
        i    += 25;
    }
    while (!(cache >> 31)) {
        cache <<= 1;
        index   = gb_skip(g, index, 1);
        i++;
    }
    index = gb_skip(g, index, 1);
    cache = gb_cache(g, index);
    if (k) {
        buf   = cache >> (32 - k);
        index = gb_skip(g, index, k);
    }
    g->index = index;
    return buf + (i << k);
}

/* Follows a packet the way loco.c does. Returns the offset of the byte in
 * which the code starts whose value overflows the decoder's sum, or -1 if
 * there is none. */
static int find_overflow(const Buf *b, const Layout *l)
{
    const uint8_t *buf = b->d;
    int size = b->n;

    for (int p = 0; p < l->nb_planes; p++) {
        GetB g = { buf, size, 0, size * 8u, size * 8u + 8 };
        RiceW r;
        int run = 0, n = l->w[p] * l->h[p], decoded;

        rice_init(&r);
        for (int i = 0; i < n; i++) {
            uint32_t v, start = g.index;
            if (run > 0) {
                run--;
                rice_update(&r, 0);
                continue;
            }
            if (g.index >= g.size)
                return -1;
            v = gb_golomb(&g, rice_k(&r));
            if (v == UINT32_MAX)
                return -1;
            if (rice_overflows(&r, (v + 1) >> 1))
                return (int)(buf - b->d) + (int)(start >> 3);
            rice_update(&r, (v + 1) >> 1);
            if (!v) {
                if (r.save >= 0) {
                    uint32_t rv = gb_golomb(&g, 2);
                    if (rv == UINT32_MAX)
                        return -1;
                    run = rv > INT_MAX ? -1 : (int)rv;
                    if (run > 1)
                        r.save += run + 1;
                    else
                        r.save -= 3;
                } else {
                    r.run2++;
                }
            } else {
                if ((((v >> 1) + l->lossy) ^ -(v & 1)) == 0x80000000u)
                    return -1;
                rice_nonzero(&r);
            }
        }
        decoded = (g.index + 7) >> 3;
        if (decoded >= size)
            return -1;
        buf  += decoded;
        size -= decoded;
    }
    return -1;
}

/* Keeps the fuzzer away from the C decoder's undefined behaviour and from
 * nothing else: a mutated packet that would overflow the sum is cut off
 * just before the code that does it. Overflowing takes more than a
 * megabyte of packet (each code adds at most 257 times its length in bits
 * to the sum), so smaller packets are not looked at. */
static void fixup(Buf *b, const Cfg *cfg)
{
    Layout l;
    int at;

    if (b->n < 1000000 || allow_ub())
        return;
    if (!layout(cfg, &l)) {
        /* A mutated real packet: its stream parameters are not in cfg.
         * (The real samples' packets are under 60 kB.) */
        b->n = 999999;
        return;
    }
    at = find_overflow(b, &l);
    if (at >= 0)
        b->n = at > 0 ? at : 1;
}

/* ---- benchmark: encode generated pictures ---- */

static int median3(int a, int b, int c)
{
    int lo = a < b ? a : b, hi = a < b ? b : a;
    return c < lo ? lo : c > hi ? hi : c;
}

/* Code values of a picture, lossless: the residual against the decoder's
 * prediction, positive ones as 2s, negative ones as -2s - 1. */
static void residuals(const uint8_t *img, int w, int h, uint32_t *vals)
{
    for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++) {
            const uint8_t *p = img + y * w + x;
            int pred, s;
            if (!y)
                pred = x ? p[-1] : 128;
            else if (!x)
                pred = p[-w];
            else
                pred = median3(p[-w], p[-w] + p[-1] - p[-w - 1], p[-1]);
            s = (int8_t)(*p - pred);
            vals[y * w + x] = s > 0 ? 2 * s : s < 0 ? -2 * s - 1 : 0;
        }
}

static int clip8(int v)
{
    return v < 0 ? 0 : v > 255 ? 255 : v;
}

/* Sum of n dice minus the mean: noise with a peak at 0. */
static int noise(int amp)
{
    return amp ? (int)(rnd(amp + 1) + rnd(amp + 1)) - amp : 0;
}

static void picture(const char *content, uint8_t *img, int w, int h, int plane)
{
    if (!strcmp(content, "flat")) {
        memset(img, 90 + 20 * plane, (size_t)w * h);
    } else if (!strcmp(content, "noise")) {
        for (int i = 0; i < w * h; i++)
            img[i] = rnd(256);
    } else if (!strcmp(content, "screen")) {
        /* Flat rectangles; some hold a two-colour pattern, like text. */
        for (int by = 0; by < h; by += 40)
            for (int bx = 0; bx < w; bx += 64) {
                int c0 = rnd(256), c1 = rnd(256), text = rnd(6) == 0;
                for (int y = by; y < by + 40 && y < h; y++)
                    for (int x = bx; x < bx + 64 && x < w; x++)
                        img[y * w + x] = text && rnd(3) == 0 ? c1 : c0;
            }
    } else {
        /* Smooth shapes plus sensor noise: little for "clean", more for
         * "photo" (which comes out at about the 6 bits per pixel of the
         * real samples). */
        int amp = !strcmp(content, "clean") ? 1 : 9;
        double ph = rnd(1000) * 0.01 + plane;
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++)
                img[y * w + x] = clip8(128 + (int)(60 * sin(x * 0.011 + y * 0.004 + ph) +
                                                   40 * sin(y * 0.031 - x * 0.007 + ph)) +
                                       noise(amp));
    }
}

/* One frame of one kind of content in one pixel format. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static Buf b;
    BitW w = { &b, 0 };
    AVPacket *pkt;
    Layout l;
    char content[16];
    const char *dash = strchr(kind, '-');
    size_t len = dash ? (size_t)(dash - kind) : 0;

    if (!dash || len >= sizeof(content))
        return NULL;
    memcpy(content, kind, len);
    content[len] = 0;
    if (strcmp(content, "flat") && strcmp(content, "noise") && strcmp(content, "screen") &&
        strcmp(content, "clean") && strcmp(content, "photo"))
        return NULL;
    if (!strcmp(dash, "-yuy2")) {
        set_extradata(cfg, 1, 1, 0);
    } else if (!strcmp(dash, "-yv12")) {
        set_extradata(cfg, 1, 5, 0);
    } else if (!strcmp(dash, "-rgb")) {
        set_extradata(cfg, 1, 3, 0);
    } else if (!strcmp(dash, "-rgbodd")) {
        /* An odd width (one less than asked for), which makes the decoder
         * shear the picture after decoding it. */
        set_extradata(cfg, 1, 3, 0);
        cfg->w = (cfg->w - 1) | 1;
    } else if (!strcmp(dash, "-rgba")) {
        set_extradata(cfg, 1, 4, 0);
    } else {
        return NULL;
    }
    if (!layout(cfg, &l))
        return NULL;

    b.n = 0;
    for (int i = 0; i < l.nb_planes; i++) {
        int n = l.w[i] * l.h[i];
        uint8_t *img   = malloc(n);
        uint32_t *vals = malloc(n * sizeof(*vals));

        picture(content, img, l.w[i], l.h[i], i);
        residuals(img, l.w[i], l.h[i], vals);
        enc_plane(&w, vals, n, 0, NULL);
        bw_align(&w, 0);
        free(vals);
        free(img);
    }
    if (b.n >= MAX_PKT - 64)
        return NULL;
    pkt = av_packet_alloc();
    av_new_packet(pkt, b.n);
    memcpy(pkt->data, b.d, b.n);
    return pkt;
}

static const uint8_t interesting[] = { 0x00, 0x01, 0x02, 0x08, 0x10, 0x40, 0x7f, 0x80, 0xc0, 0xfe, 0xff };

const CodecOps ops_loco = {
    .name           = "loco",
    .c_name         = "loco",
    .rs_name        = "loco_rs",
    .id             = AV_CODEC_ID_LOCO,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .fixup          = fixup,
    .synth_kinds    = "flat-yuy2 screen-yuy2 clean-yuy2 photo-yuy2 noise-yuy2 photo-yv12 "
                      "photo-rgb photo-rgba photo-rgbodd",
    .synth          = synth,
};
