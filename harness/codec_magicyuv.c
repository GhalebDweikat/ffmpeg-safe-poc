/* MagicYUV: packet generators.
 *
 * A packet carries everything the decoder needs (format, size, slice
 * layout, Huffman code lengths), so the generator is an encoder of sorts:
 * it draws residuals, picks code lengths, and writes slices with the codes
 * the decoder will derive from those lengths. For fuzzing, each of these
 * steps can go wrong on purpose. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libavutil/intreadwrite.h"
#include "libavutil/macros.h"
#include "libavutil/mem.h"

#include "common.h"

static const struct Format {
    uint8_t code;
    int planes, bps, hshift, vshift;
} formats[] = {
    { 0x6b, 1,  8, 0, 0 },      /* gray8 */
    { 0x69, 3,  8, 1, 1 },      /* yuv420p */
    { 0x68, 3,  8, 1, 0 },      /* yuv422p */
    { 0x67, 3,  8, 0, 0 },      /* yuv444p */
    { 0x6a, 4,  8, 0, 0 },      /* yuva444p */
    { 0x65, 3,  8, 0, 0 },      /* gbrp */
    { 0x66, 4,  8, 0, 0 },      /* gbrap */
    { 0x73, 1, 10, 0, 0 },      /* gray10 */
    { 0x7b, 3, 10, 1, 1 },      /* yuv420p10 */
    { 0x6c, 3, 10, 1, 0 },      /* yuv422p10 */
    { 0x76, 3, 10, 0, 0 },      /* yuv444p10 */
    { 0x6d, 3, 10, 0, 0 },      /* gbrp10 */
    { 0x6e, 4, 10, 0, 0 },      /* gbrap10 */
    { 0x6f, 3, 12, 0, 0 },      /* gbrp12 */
    { 0x70, 4, 12, 0, 0 },      /* gbrap12 */
    { 0x71, 3, 14, 0, 0 },      /* gbrp14 */
    { 0x72, 4, 14, 0, 0 },      /* gbrap14 */
};
#define NB_FORMATS ((int)FF_ARRAY_ELEMS(formats))
#define MAX_SYMS (1 << 14)

/* How code lengths are chosen. */
enum {
    TABLE_COUNTS,       /* Huffman code for the residuals actually drawn */
    TABLE_COUNTS12,     /* the same, at most 12 bits long, as encoders write */
    TABLE_FLAT,         /* every code as long as a sample */
    TABLE_RANDOM,       /* Huffman code for random weights */
    TABLE_HEAVY,        /* a few very likely symbols: short codes and long ones */
    TABLE_CHAIN,        /* one code of every length up to 32 */
    NB_TABLES,
};

typedef struct Params {
    int format;                 /* index into formats */
    int w, h, slice_height;
    int interlaced;
    int table;                  /* TABLE_*, the same for all planes */
    int pred;                   /* 1..3, or 0 for a random one per slice */
    int raw;                    /* 1 in this many slices is stored raw; 0 for none */
    int spread;                 /* how wide the residuals are spread */
    int damage;                 /* fuzzing: allow things to be wrong */
} Params;

/* Output: a growing byte buffer with a bit writer on top. */
typedef struct Out {
    uint8_t *d;
    size_t n, cap;
    uint64_t acc;
    int nbits;
    size_t head;                /* where the slices start */
} Out;

static void out_byte(Out *o, int v)
{
    if (o->n == o->cap) {
        o->cap = o->cap ? 2 * o->cap : 1 << 16;
        o->d   = realloc(o->d, o->cap);
        if (!o->d)
            abort();
    }
    o->d[o->n++] = (uint8_t)v;
}

static void out_le32(Out *o, uint32_t v)
{
    for (int i = 0; i < 4; i++)
        out_byte(o, v >> (8 * i));
}

static void out_bits(Out *o, int len, uint32_t value)
{
    o->acc    = o->acc << len | value;
    o->nbits += len;
    while (o->nbits >= 8) {
        o->nbits -= 8;
        out_byte(o, (int)(o->acc >> o->nbits));
    }
}

static void out_flush(Out *o)
{
    if (o->nbits)
        out_bits(o, 8 - o->nbits, 0);
}

/* Huffman code lengths for n weights (all at least 1), by merging the two
 * lightest nodes; the leaves are sorted first so two queues do. */
static const uint32_t *sort_weights;
static int cmp_weight(const void *a, const void *b)
{
    uint32_t x = sort_weights[*(const int *)a], y = sort_weights[*(const int *)b];
    return x != y ? (x > y) - (x < y) : *(const int *)a - *(const int *)b;
}

static int huffman_lengths(const uint32_t *weight, int n, uint8_t *len)
{
    static int order[MAX_SYMS], parent[2 * MAX_SYMS], depth[2 * MAX_SYMS];
    static uint64_t w[2 * MAX_SYMS];
    int leaf = 0, node = n, next = n, max = 0;

    for (int i = 0; i < n; i++)
        order[i] = i;
    sort_weights = weight;
    qsort(order, n, sizeof(order[0]), cmp_weight);
    for (int i = 0; i < n; i++)
        w[i] = weight[order[i]];
    while (next < 2 * n - 1) {
        int pick[2];
        for (int k = 0; k < 2; k++)
            pick[k] = leaf < n && (node >= next || w[leaf] <= w[node]) ? leaf++ : node++;
        w[next] = w[pick[0]] + w[pick[1]];
        parent[pick[0]] = parent[pick[1]] = next++;
    }
    depth[2 * n - 2] = 0;
    for (int i = 2 * n - 3; i >= 0; i--)
        depth[i] = depth[parent[i]] + 1;
    for (int i = 0; i < n; i++) {
        len[order[i]] = (uint8_t)FFMIN(depth[i], 255);
        max = FFMAX(max, depth[i]);
    }
    return max;
}

/* The codes the decoder derives from a set of lengths: longest first,
 * symbols ascending within a length, counting upwards. */
static void assign_codes(const uint8_t *len, int n, uint32_t *code)
{
    uint64_t next = 0;

    for (int l = 32; l >= 1; l--)
        for (int s = 0; s < n; s++)
            if (len[s] == l) {
                code[s] = (uint32_t)(next >> (32 - l));
                next   += 1ULL << (32 - l);
            }
}

/* A residual: small values are likely, in both directions. */
static int residual(int max, int spread)
{
    int m = 0, r;

    while (m < 40 && rnd(2))
        m++;
    r = m * spread + (spread > 1 ? rnd(spread) : 0);
    if (rnd(2))
        r = -r;
    return r & (max - 1);
}

static void choose_lengths(const Params *p, int max, int bps, const uint32_t *count, uint8_t *len, int damage)
{
    static uint32_t weight[MAX_SYMS];
    int table = p->table;

    switch (table) {
    case TABLE_COUNTS:
    case TABLE_COUNTS12:
        /* Lengthen the rare symbols' weights until the code is short enough. */
        for (int bias = 1; ; bias *= 4) {
            int limit = table == TABLE_COUNTS12 ? FFMAX(12, bps) : 32;
            for (int s = 0; s < max; s++)
                weight[s] = count[s] + bias;
            if (huffman_lengths(weight, max, len) <= limit)
                break;
        }
        break;
    case TABLE_RANDOM: {
        int bits = 1 + rnd(18);
        for (int s = 0; s < max; s++)
            weight[s] = 1 + rnd(1 << rnd(bits));
        if (huffman_lengths(weight, max, len) > 32)
            memset(len, bps, max);
        break;
    }
    case TABLE_HEAVY: {
        int heavy = 1 + rnd(12);
        for (int s = 0; s < max; s++)
            weight[s] = 1;
        for (int k = 0; k < heavy; k++)
            weight[k ? rnd(max) : 0] = 1 << (8 + rnd(13));
        if (huffman_lengths(weight, max, len) > 32)
            memset(len, bps, max);
        break;
    }
    case TABLE_CHAIN: {
        /* One code of length bps is replaced by codes of bps + 1 ... 32
         * bits and one more of 32; pairs of others are merged to make
         * room for the extra symbols. */
        int s = 0, first = rnd(max);
        uint8_t l[MAX_SYMS];
        l[s++] = 32;
        for (int k = 32; k > bps; k--)
            l[s++] = (uint8_t)k;
        for (int k = 0; k < 32 - bps; k++)
            l[s++] = (uint8_t)(bps - 1);
        while (s < max)
            l[s++] = (uint8_t)bps;
        for (s = 0; s < max; s++)
            len[(s + first) % max] = l[s];
        break;
    }
    default:
        memset(len, bps, max);
        break;
    }

    if (damage)
        for (int k = 1 + rnd(3); k > 0; k--) {
            int s = rnd(max);
            switch (rnd(5)) {
            case 0:  len[s] = (uint8_t)(len[s] + 1);  break;
            case 1:  len[s] = (uint8_t)(len[s] - 1);  break;
            case 2:  len[s] = (uint8_t)(1 + rnd(32)); break;
            case 3:  len[s] = (uint8_t)rnd(128);      break;
            default: len[rnd(max)] = len[s];          break;
            }
        }
}

/* The code lengths as the packet stores them: a length, or a length with
 * the top bit set followed by a repeat count. */
static void write_lengths(Out *o, const uint8_t *len, int max, int damage)
{
    for (int s = 0; s < max;) {
        int run = 1;
        while (s + run < max && run < 256 && len[s + run] == len[s])
            run++;
        if (damage && rnd(3) == 0)
            run = 1 + rnd(run);
        if (run == 1 && !(damage && rnd(8) == 0)) {
            out_byte(o, len[s] & 0x7f);
        } else {
            out_byte(o, len[s] | 0x80);
            out_byte(o, run - 1);
        }
        s += run;
    }
}

/* Build one packet. */
static void build(Out *o, const Params *p)
{
    static uint16_t *sym[4];
    static size_t sym_cap[4];
    static uint8_t len[4][MAX_SYMS];
    static uint32_t code[4][MAX_SYMS];
    static uint32_t count[MAX_SYMS];
    const struct Format *f = &formats[p->format];
    int max = 1 << f->bps, planes = f->planes;
    int slice_height = p->slice_height > 0 ? p->slice_height : 1;
    int nb_slices = (p->h + slice_height - 1) / slice_height;
    int pw[4], ph[4], sh[4];
    size_t offsets;
    int header_size = 32;
    int damage_lengths = p->damage && rnd(10) == 0;

    o->n = 0;
    o->acc = 0;
    o->nbits = 0;
    if (nb_slices < 1 || nb_slices > 4096)
        nb_slices = 1;

    if (p->damage && rnd(40) == 0)
        header_size = rnd(2) ? rnd(64) : 32 + 4 * rnd(8);

    out_byte(o, 'M'); out_byte(o, 'A'); out_byte(o, 'G'); out_byte(o, 'Y');
    out_le32(o, header_size);
    out_byte(o, p->damage && rnd(60) == 0 ? rnd(256) : 7);
    out_byte(o, p->damage && rnd(60) == 0 ? 0x60 + rnd(32) : f->code);
    out_byte(o, 12);
    out_byte(o, p->damage ? rnd(4) : 0);                        /* colour matrix */
    out_byte(o, (p->interlaced ? 2 : 0) | (p->damage ? rnd(2) * 4 | rnd(2) : 0));
    out_byte(o, 0); out_byte(o, 32); out_byte(o, 0);
    out_le32(o, p->w);
    out_le32(o, p->h);
    out_le32(o, p->damage && rnd(60) == 0 ? rnd(300) : p->w);
    out_le32(o, p->slice_height);
    out_le32(o, 0);

    offsets = o->n;
    for (int i = 0; i < planes * nb_slices; i++)
        out_le32(o, 0);
    out_byte(o, p->damage && rnd(60) == 0 ? rnd(6) : planes);
    for (int i = 0; i < planes * nb_slices; i++)
        out_byte(o, 0);

    /* Residuals and code lengths for every plane. */
    for (int i = 0; i < planes; i++) {
        size_t n;
        int chroma = i == 1 || i == 2;
        pw[i] = AV_CEIL_RSHIFT(p->w, chroma ? f->hshift : 0);
        ph[i] = AV_CEIL_RSHIFT(p->h, chroma ? f->vshift : 0);
        sh[i] = AV_CEIL_RSHIFT(slice_height, chroma ? f->vshift : 0);
        n = (size_t)pw[i] * ph[i];
        if (n > sym_cap[i]) {
            sym_cap[i] = n;
            sym[i] = realloc(sym[i], n * sizeof(uint16_t));
            if (!sym[i])
                abort();
        }
        memset(count, 0, max * sizeof(count[0]));
        for (size_t k = 0; k < n; k++) {
            sym[i][k] = (uint16_t)residual(max, p->spread);
            count[sym[i][k]]++;
        }
        choose_lengths(p, max, f->bps, count, len[i], damage_lengths && rnd(2));
        assign_codes(len[i], max, code[i]);
        if (p->table > TABLE_FLAT) {
            /* The code was not made for these residuals: use the symbols
             * with the shortest codes most of the time, so that rows are
             * not all long codes. */
            for (size_t k = 0; k < n; k++) {
                int s = rnd(max);
                for (int t = rnd(4); t > 0; t--) {
                    int c = rnd(max);
                    if (len[i][c] < len[i][s])
                        s = c;
                }
                sym[i][k] = (uint16_t)s;
            }
        }
        if (!(p->damage && rnd(80) == 0))
            write_lengths(o, len[i], max, p->damage);
    }
    if (p->damage && rnd(30) == 0)
        for (int k = rnd(6); k > 0; k--)
            out_byte(o, rnd(256));
    o->head = o->n;

    /* Slices, in the order FFmpeg's encoder writes them. */
    for (int j = 0; j < nb_slices; j++) {
        for (int i = 0; i < planes; i++) {
            int rows  = FFMIN(sh[i], ph[i] - j * sh[i]);
            int pred  = p->pred ? p->pred : 1 + rnd(3);
            int raw   = p->raw && rnd(p->raw) == 0;
            size_t n, first = (size_t)j * sh[i] * pw[i];
            uint32_t off = (uint32_t)(o->n - header_size);

            if (p->damage && rnd(200) == 0 && o->n > 40)
                off -= rnd(8);                   /* overlap the previous slice */
            AV_WL32(o->d + offsets + 4 * (i * nb_slices + j), off);
            if (rows < 0)
                rows = 0;
            n = (size_t)rows * pw[i];
            if (p->damage) {
                if (rnd(24) == 0)
                    pred = rnd(256);
                if (rnd(16) == 0)
                    n = rnd(n + 1);              /* too short */
                else if (rnd(40) == 0)
                    n += 1 + rnd(9);             /* too long */
            }
            if (first + n > (size_t)pw[i] * ph[i])
                n = (size_t)pw[i] * ph[i] - FFMIN(first, (size_t)pw[i] * ph[i]);

            out_byte(o, raw | (p->damage && rnd(30) == 0 ? rnd(256) & ~1 : 0));
            out_byte(o, pred);
            if (raw) {
                for (size_t k = 0; k < n; k++)
                    out_bits(o, f->bps, sym[i][first + k]);
            } else {
                for (size_t k = 0; k < n; k++) {
                    int s = sym[i][first + k];
                    int l = len[i][s];
                    if (l >= 1 && l <= 32)
                        out_bits(o, l, code[i][s]);
                    else
                        out_bits(o, 1 + rnd(8), rnd(256));
                }
            }
            out_flush(o);
            if (p->damage) {
                /* Encoders pad slices to 4 bytes with zeros. */
                switch (rnd(4)) {
                case 0: while (o->n & 3) out_byte(o, 0);                    break;
                case 1: for (int k = rnd(5); k > 0; k--) out_byte(o, rnd(256)); break;
                }
            } else {
                while (o->n & 3)
                    out_byte(o, 0);
            }
        }
    }
}

/* Test hook in the Rust library: 0 forces the portable predictors. */
void magicyuv_rs_set_simd(int enabled);

/* A quarter of the cases, generated or from seed files, run the Rust
 * decoder without AVX2. */
static void options(Cfg *cfg)
{
    magicyuv_rs_set_simd(rnd(4) != 0);
}

/* The last generated packet up to the end of its code lengths. */
static uint8_t head[1 << 16];
static int head_n;

static void config(Cfg *cfg)
{
    /* opt[0]: format index + 1; opt[1]: interlaced; opt[2]: table kind + 1. */
    cfg->opt[0] = 1 + (rnd(5) < 3 ? rnd(7) : rnd(NB_FORMATS));
    cfg->opt[1] = rnd(4) == 0;
    cfg->opt[2] = 1 + rnd(NB_TABLES);
}

static void gen(Buf *b, const Cfg *cfg)
{
    static Out o;
    Params p = { 0 };
    const struct Format *f;
    int align;

    /* Mostly the same format and size for a whole case, so that frames
     * are reused and what a slice leaves unwritten shows. */
    p.format     = rnd(12) ? cfg->opt[0] - 1 : (int)rnd(NB_FORMATS);
    p.interlaced = rnd(12) ? cfg->opt[1] : (int)rnd(2);
    p.table      = rnd(4)  ? cfg->opt[2] - 1 : (int)rnd(NB_TABLES);
    if (p.format < 0)
        p.format = 0;
    f     = &formats[p.format];
    p.w   = rnd(24) ? cfg->w : 1 + (int)rnd(40);
    p.h   = rnd(24) ? cfg->h : 1 + (int)rnd(40);
    align = 1 << f->vshift;
    switch (rnd(4)) {
    case 0:  p.slice_height = p.h;                break;
    case 1:  p.slice_height = (p.h + 1) / 2;      break;
    default: p.slice_height = 1 + rnd(p.h);       break;
    }
    if (rnd(30))
        p.slice_height = FFALIGN(p.slice_height, align);
    if (p.interlaced && rnd(30)) {
        /* Two rows of every plane in each slice, the last one included. */
        p.slice_height = FFMAX(p.slice_height, 2 * align);
        while (p.slice_height < p.h && p.h % p.slice_height &&
               (p.h % p.slice_height) >> f->vshift < 2)
            p.slice_height += align;
    }
    if (rnd(100) == 0)
        p.slice_height = rnd(3) ? rnd(8) : -(int)rnd(4);
    p.pred   = rnd(3) ? 0 : 1 + rnd(3);
    p.raw    = rnd(3) ? 0 : 1 + rnd(6);
    p.spread = 1 << rnd(f->bps - 3);
    p.damage = 1;

    build(&o, &p);
    b->n = (int)FFMIN(o.n, (size_t)MAX_PKT);
    memcpy(b->d, o.d, b->n);
    head_n = (int)FFMIN(o.head, sizeof(head));
    memcpy(head, o.d, head_n);
}

/* Mutations of the header mostly end at the first check; put the magic
 * and version back most of the time. */
static void fixup(Buf *b, const Cfg *cfg)
{
    if (b->n >= 36 && rnd(4)) {
        memcpy(b->d, "MAGY", 4);
        b->d[8] = 7;
    }
    /* A changed code length nearly always makes the lengths invalid, and
     * in small packets they are most of the bytes. For generated packets,
     * undo changes to the header and the lengths half of the time, so that
     * more damaged slices get decoded. */
    if (head_n && b->n >= head_n && rnd(2))
        memcpy(b->d, head, head_n);
}

/* The Rust entry points, called directly to count why packets are refused
 * (the fuzzer prints the counts with its result). */
typedef struct MagicyuvRs MagicyuvRs;
typedef struct MagicyuvRsHeader {
    int32_t format, color_matrix, flags, width, height;
    int64_t detail;
} MagicyuvRsHeader;
MagicyuvRs *magicyuv_rs_new(void);
void magicyuv_rs_free(MagicyuvRs *dec);
int magicyuv_rs_header(MagicyuvRs *dec, const uint8_t *pkt, size_t pkt_size, MagicyuvRsHeader *out);
int magicyuv_rs_tables(MagicyuvRs *dec, const uint8_t *pkt, size_t pkt_size,
                       int coded_width, int coded_height, int64_t *detail);

static const char *const status_names[] = {
    "accepted", "invalid", "header_size", "version", "format", "slice_width",
    "slice_height", "slice_height_align", "slice_count", "impossible_slice_height",
    "impossible_height", "huffman_codes", "huffman_build", "huffman_short",
    "accepted_no_frame",
};

static void *direct_open(const Cfg *cfg)
{
    return magicyuv_rs_new();
}

static int direct_step(void *st, const Buf *b, const AVFrame *rs_frame,
                       int rs_got_frame, char *why, size_t why_size)
{
    MagicyuvRsHeader h;
    int64_t detail;
    int ret = magicyuv_rs_header(st, b->d, b->n, &h);

    if (!ret)
        ret = magicyuv_rs_tables(st, b->d, b->n, h.width, h.height, &detail);
    if (ret < 0 || ret > 13 || (ret && rs_got_frame)) {
        snprintf(why, why_size, "direct call returned %d, frame %d", ret, rs_got_frame);
        return -1;
    }
    /* Accepted here but no frame: the picture size was refused by FFmpeg. */
    return !ret && !rs_got_frame ? 14 : ret;
}

static void direct_close(void *st)
{
    magicyuv_rs_free(st);
}

/* Benchmark: one predictor, sample size and code shape per stream. */
static AVPacket *synth(const char *kind, Cfg *cfg)
{
    static const struct {
        const char *name;
        int format, pred, table, raw, spread, interlaced;
    } kinds[] = {
        { "left8",        3, 1, TABLE_COUNTS12, 0,  2, 0 },
        { "gradient8",    3, 2, TABLE_COUNTS12, 0,  2, 0 },
        { "median8",      3, 3, TABLE_COUNTS12, 0,  2, 0 },
        { "left8-noisy",  3, 1, TABLE_COUNTS12, 0, 16, 0 },
        { "left8-rgb",    5, 1, TABLE_COUNTS12, 0,  2, 0 },
        { "median8-int",  3, 3, TABLE_COUNTS12, 0,  2, 1 },
        { "raw8",         3, 1, TABLE_FLAT,     1,  2, 0 },
        { "longcodes8",   3, 1, TABLE_HEAVY,    0,  2, 0 },
        { "left10",      10, 1, TABLE_COUNTS12, 0,  4, 0 },
        { "gradient10",  10, 2, TABLE_COUNTS12, 0,  4, 0 },
        { "median10",    10, 3, TABLE_COUNTS12, 0,  4, 0 },
        { "left12-rgb",  13, 1, TABLE_COUNTS12, 0, 16, 0 },
        { "raw10",       10, 1, TABLE_FLAT,     1,  4, 0 },
    };
    static Out o;
    AVPacket *pkt;

    magicyuv_rs_set_simd(!getenv("MAGICYUV_NO_SIMD"));
    for (size_t k = 0; k < FF_ARRAY_ELEMS(kinds); k++) {
        if (!strcmp(kind, kinds[k].name)) {
            Params p = {
                .format = kinds[k].format, .w = cfg->w, .h = cfg->h,
                .slice_height = cfg->h, .interlaced = kinds[k].interlaced,
                .table = kinds[k].table, .pred = kinds[k].pred,
                .raw = kinds[k].raw, .spread = kinds[k].spread,
            };
            build(&o, &p);
            pkt = av_packet_alloc();
            if (!pkt || av_new_packet(pkt, (int)o.n) < 0)
                abort();
            memcpy(pkt->data, o.d, o.n);
            return pkt;
        }
    }
    return NULL;
}

static const uint8_t interesting[] = {
    0x00, 0x01, 0x02, 0x03, 0x04, 0x07, 0x08, 0x0c, 0x0d, 0x20, 0x21, 0x7f,
    0x80, 0x81, 0x88, 0x8c, 0xa0, 0xff, 0x65, 0x69, 0x6b, 0x6d, 0x73, 0x7b,
};

const CodecOps ops_magicyuv = {
    .name           = "magicyuv",
    .c_name         = "magicyuv",
    .rs_name        = "magicyuv_rs",
    .id             = AV_CODEC_ID_MAGICYUV,
    .interesting    = interesting,
    .nb_interesting = sizeof(interesting),
    .config         = config,
    .gen            = gen,
    .options        = options,
    .fixup          = fixup,
    .direct_open    = direct_open,
    .direct_step    = direct_step,
    .direct_close   = direct_close,
    .status_names   = status_names,
    .nb_status      = FF_ARRAY_ELEMS(status_names),
    .synth_kinds    = "left8 gradient8 median8 left8-noisy left8-rgb median8-int raw8 longcodes8 "
                      "left10 gradient10 median10 left12-rgb raw10",
    .synth          = synth,
};
