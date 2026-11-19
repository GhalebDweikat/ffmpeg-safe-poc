//! Dequantisation, inverse DCT and clipping of one 8x8 block.
//!
//! FFmpeg has three versions of this, and they do not agree on extreme
//! input, so all three are here:
//!
//! * [`Idct::C12`]: the C code for 12-bit output (`prores_idct_12` in
//!   `proresdsp.c`, built from `simple_idct_template.c`). FFmpeg has no
//!   assembly for 12 bits.
//! * [`Idct::Asm10`]: the x86-64 SSE2/AVX assembly for 10-bit output
//!   (`x86/proresdsp.asm` and `x86/simple_idct10_template.asm`). This is
//!   what FFmpeg runs on any x86-64 machine.
//! * [`Idct::C10`]: the C code for 10-bit output (`prores_idct_10`), which
//!   FFmpeg runs when it is built without assembly.
//!
//! Each exists twice: in portable Rust ([`idct_block`]) and with AVX2
//! intrinsics ([`idct_block_avx2`]). All arithmetic is in wrapping 32-bit
//! integers, as in the originals.
//!
//! Blocks are not stored in natural order but in the order the AVX2 code
//! wants to load them (see [`PERM`]); the coefficient scan tables and the
//! quantisation matrices are permuted to match, the way FFmpeg permutes
//! them for its own assembly.

/// Which of FFmpeg's inverse DCTs to reproduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idct {
    /// 10-bit output, the C code.
    C10,
    /// 10-bit output, the x86-64 assembly.
    Asm10,
    /// 12-bit output, the C code.
    C12,
}

impl Idct {
    /// The largest sample value the block is clipped to; the smallest is 4.
    pub fn clip_max(self) -> i16 {
        match self {
            Idct::C10 | Idct::Asm10 => (1 << 10) - 4 - 1,
            Idct::C12 => (1 << 12) - 4 - 1,
        }
    }
}

const CLIP_MIN: i16 = 4;

/// cos(i * pi / 16) * sqrt(2) * 2^14, and 2^15 for 12 bits.
const W10: [i32; 8] = [0, 22725, 21407, 19265, 16384, 12873, 8867, 4520];
const W12: [i32; 8] = [0, 45451, 42813, 38531, 32767, 25746, 17734, 9041];

/// Where the coefficient with natural index `8 * row + column` is stored.
///
/// The row pass multiplies and adds coefficients of a row in the pairs
/// (0, 2), (4, 6), (1, 3) and (5, 7). A block is stored as four runs of 16
/// values, one per pair, each holding that pair for the eight rows in
/// turn, so that one 32-byte load gives one pair for all rows.
pub const PERM: [u8; 64] = {
    let mut p = [0u8; 64];
    let mut n = 0;
    while n < 64 {
        let (row, col) = (n / 8, n % 8);
        let run = match col {
            0 | 2 => 0,
            4 | 6 => 1,
            1 | 3 => 2,
            _ => 3,
        };
        p[n] = (run * 16 + 2 * row + (col >> 1 & 1)) as u8;
        n += 1;
    }
    p
};

/// A table indexed in natural order, moved to storage order.
pub const fn permuted(natural: &[u8; 64]) -> [u8; 64] {
    let mut out = [0u8; 64];
    let mut n = 0;
    while n < 64 {
        out[PERM[n] as usize] = natural[n];
        n += 1;
    }
    out
}

/// One 8-point inverse DCT without the final shift. `bias` is added to
/// every output.
#[inline(always)]
fn idct_1d(w: &[i32; 8], x: [i32; 8], bias: i32) -> [i32; 8] {
    let m = |a: i32, b: i32| a.wrapping_mul(b);
    let e = m(w[4], x[0]).wrapping_add(bias);
    let a0 = e.wrapping_add(m(w[2], x[2])).wrapping_add(m(w[4], x[4])).wrapping_add(m(w[6], x[6]));
    let a1 = e.wrapping_add(m(w[6], x[2])).wrapping_sub(m(w[4], x[4])).wrapping_sub(m(w[2], x[6]));
    let a2 = e.wrapping_sub(m(w[6], x[2])).wrapping_sub(m(w[4], x[4])).wrapping_add(m(w[2], x[6]));
    let a3 = e.wrapping_sub(m(w[2], x[2])).wrapping_add(m(w[4], x[4])).wrapping_sub(m(w[6], x[6]));
    let b0 = m(w[1], x[1]).wrapping_add(m(w[3], x[3])).wrapping_add(m(w[5], x[5])).wrapping_add(m(w[7], x[7]));
    let b1 = m(w[3], x[1]).wrapping_sub(m(w[7], x[3])).wrapping_sub(m(w[1], x[5])).wrapping_sub(m(w[5], x[7]));
    let b2 = m(w[5], x[1]).wrapping_sub(m(w[1], x[3])).wrapping_add(m(w[7], x[5])).wrapping_add(m(w[3], x[7]));
    let b3 = m(w[7], x[1]).wrapping_sub(m(w[5], x[3])).wrapping_add(m(w[3], x[5])).wrapping_sub(m(w[1], x[7]));
    [
        a0.wrapping_add(b0),
        a1.wrapping_add(b1),
        a2.wrapping_add(b2),
        a3.wrapping_add(b3),
        a3.wrapping_sub(b3),
        a2.wrapping_sub(b2),
        a1.wrapping_sub(b1),
        a0.wrapping_sub(b0),
    ]
}

fn saturate(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// The row pass over a dequantised block in natural order, in place.
fn rows(kind: Idct, block: &mut [i16; 64]) {
    for row in block.as_chunks_mut::<8>().0 {
        let mut x = row.map(i32::from);
        match kind {
            Idct::C10 => {
                // ROW_SHIFT 13 plus the extra shift of 2.
                *row = idct_1d(&W10, x, 1 << 14).map(|v| (v >> 15) as i16);
            }
            Idct::Asm10 => {
                // The assembly rounds by adding 1 to the first coefficient,
                // in 16 bits, and packs the results with signed saturation.
                x[0] = i32::from(row[0].wrapping_add(1));
                *row = idct_1d(&W10, x, 0).map(|v| saturate(v >> 15));
            }
            Idct::C12 => {
                if row[1..] == [0; 7] {
                    // The DC-only shortcut. Not the same as the full
                    // transform, because W4 is 32767 and not 32768.
                    *row = [((x[0] + 1) >> 1) as i16; 8];
                } else {
                    *row = idct_1d(&W12, x, 1 << 15).map(|v| (v >> 16) as i16);
                }
            }
        }
    }
}

/// The column pass, in place.
fn columns(kind: Idct, block: &mut [i16; 64]) {
    for c in 0..8 {
        let mut x = [0i32; 8];
        for (r, v) in x.iter_mut().enumerate() {
            *v = i32::from(block[8 * r + c]);
        }
        let out = match kind {
            Idct::C10 => {
                // 8192 is added in 16 bits, the rounding term 8 in 32.
                x[0] = i32::from(block[c].wrapping_add(8192)) + 8;
                idct_1d(&W10, x, 0).map(|v| (v >> 18) as i16)
            }
            Idct::Asm10 => {
                // Both terms are added in 16 bits.
                x[0] = i32::from(block[c].wrapping_add(8192 + 8));
                idct_1d(&W10, x, 0).map(|v| saturate(v >> 18))
            }
            Idct::C12 => {
                x[0] = i32::from(block[c].wrapping_add(8192)) + 2;
                idct_1d(&W12, x, 0).map(|v| (v >> 17) as i16)
            }
        };
        for (r, v) in out.into_iter().enumerate() {
            block[8 * r + c] = v;
        }
    }
}

/// `idct_put` without the store: dequantise `block` with `qmat` (both in
/// storage order), transform it, and clip the result to the sample range.
/// Returns the eight rows of samples.
pub fn idct_block(kind: Idct, block: &[i16; 64], qmat: &[i16; 64]) -> [[u16; 8]; 8] {
    let mut natural = [0i16; 64];
    for (n, p) in natural.iter_mut().zip(PERM) {
        *n = block[usize::from(p)].wrapping_mul(qmat[usize::from(p)]);
    }
    rows(kind, &mut natural);
    columns(kind, &mut natural);
    let max = kind.clip_max();
    let mut out = [[0u16; 8]; 8];
    for (o, n) in out.iter_mut().zip(natural.as_chunks::<8>().0) {
        *o = n.map(|v| v.clamp(CLIP_MIN, max) as u16);
    }
    out
}

/// [`idct_block`] with AVX2. Same result.
///
/// The row pass works on all eight rows at once, one row per 32-bit lane:
/// each product pair of the transform is one `pmaddwd`. The results are
/// regrouped into the same pairs for the column pass, one column per lane.
/// FFmpeg's assembly does the same with 128-bit registers, four lanes at a
/// time.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub fn idct_block_avx2(kind: Idct, block: &[i16; 64], qmat: &[i16; 64]) -> [[u16; 8]; 8] {
    use core::arch::x86_64::*;

    let (b, q) = (block.as_chunks::<16>().0, qmat.as_chunks::<16>().0);
    let dequant = |i: usize| _mm256_mullo_epi16(avx2::load(&b[i]), avx2::load(&q[i]));
    let (mut p02, p46, p13, p57) = (dequant(0), dequant(1), dequant(2), dequant(3));
    let low_word = |v: i32| _mm256_set1_epi32(v);

    let packed = match kind {
        Idct::Asm10 => {
            p02 = _mm256_add_epi16(p02, low_word(1));
            let z = avx2::pass::<15>(&W10, false, _mm256_setzero_si256(), p02, p46, p13, p57);
            z.map(|(s, d)| _mm256_packs_epi32(s, d))
        }
        Idct::C10 => {
            let z = avx2::pass::<15>(&W10, false, _mm256_set1_epi32(1 << 14), p02, p46, p13, p57);
            // Keep the low 16 bits instead of saturating.
            let wrap = |v| _mm256_srai_epi32::<16>(_mm256_slli_epi32::<16>(v));
            z.map(|(s, d)| _mm256_packs_epi32(wrap(s), wrap(d)))
        }
        Idct::C12 => {
            // Rows with nothing but a DC coefficient take the shortcut.
            let high_words = _mm256_set1_epi32(-65536);
            let ac = _mm256_or_si256(
                _mm256_or_si256(p46, p13),
                _mm256_or_si256(p57, _mm256_and_si256(p02, high_words)),
            );
            let dc_only = _mm256_cmpeq_epi32(ac, _mm256_setzero_si256());
            let dc = _mm256_srai_epi32::<16>(_mm256_slli_epi32::<16>(p02));
            let dc = _mm256_srai_epi32::<1>(_mm256_add_epi32(dc, _mm256_set1_epi32(1)));
            let z = avx2::pass::<16>(&W12, true, _mm256_set1_epi32(1 << 15), p02, p46, p13, p57);
            let pick = |v| _mm256_blendv_epi8(v, dc, dc_only);
            z.map(|(s, d)| _mm256_packs_epi32(pick(s), pick(d)))
        }
    };

    let (mut p02, p46, p13, p57) = avx2::regroup(packed);
    let z = match kind {
        Idct::Asm10 => {
            p02 = _mm256_add_epi16(p02, low_word(8192 + 8));
            avx2::pass::<18>(&W10, false, _mm256_setzero_si256(), p02, p46, p13, p57)
        }
        Idct::C10 => {
            p02 = _mm256_add_epi16(p02, low_word(8192));
            avx2::pass::<18>(&W10, false, _mm256_set1_epi32(8 * W10[4]), p02, p46, p13, p57)
        }
        Idct::C12 => {
            p02 = _mm256_add_epi16(p02, low_word(8192));
            avx2::pass::<17>(&W12, true, _mm256_set1_epi32(2 * W12[4]), p02, p46, p13, p57)
        }
    };

    let min = _mm256_set1_epi16(CLIP_MIN);
    let max = _mm256_set1_epi16(kind.clip_max());
    let mut out = [[0u16; 8]; 8];
    for (y, (s, d)) in z.into_iter().enumerate() {
        // Rows y and 7 - y, each as two halves; put the halves together.
        let v = _mm256_permute4x64_epi64::<0xD8>(_mm256_packs_epi32(s, d));
        let v = _mm256_min_epi16(_mm256_max_epi16(v, min), max);
        let (top, bottom) = avx2::rows(v);
        out[y] = top;
        out[7 - y] = bottom;
    }
    out
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use core::arch::x86_64::*;

    /// Sixteen 16-bit values as a vector.
    #[target_feature(enable = "avx2")]
    #[inline]
    pub fn load(w: &[i16; 16]) -> __m256i {
        let q = |i: usize| {
            let word = |k: usize| u64::from(w[i + k] as u16) << (16 * k);
            (word(0) | word(1) | word(2) | word(3)) as i64
        };
        _mm256_set_epi64x(q(12), q(8), q(4), q(0))
    }

    /// A vector as two rows of eight samples.
    #[target_feature(enable = "avx2")]
    #[inline]
    pub fn rows(v: __m256i) -> ([u16; 8], [u16; 8]) {
        let row = |a: i64, b: i64| {
            let (a, b) = (a as u64, b as u64);
            [
                a as u16,
                (a >> 16) as u16,
                (a >> 32) as u16,
                (a >> 48) as u16,
                b as u16,
                (b >> 16) as u16,
                (b >> 32) as u16,
                (b >> 48) as u16,
            ]
        };
        (
            row(_mm256_extract_epi64::<0>(v), _mm256_extract_epi64::<1>(v)),
            row(_mm256_extract_epi64::<2>(v), _mm256_extract_epi64::<3>(v)),
        )
    }

    /// One pass of the transform over eight lanes.
    ///
    /// `p02` holds, in each 32-bit lane, inputs 0 (low half) and 2 (high
    /// half) of one 8-point transform; `p46`, `p13` and `p57` likewise.
    /// Returns, for k = 0..4, outputs k and 7 - k, shifted right by `SHIFT`.
    ///
    /// With `large`, some of the constants do not fit in 16 signed bits.
    /// `pmaddwd` then multiplies by the constant minus 65536, and the
    /// input times 65536 is added afterwards.
    #[target_feature(enable = "avx2")]
    #[inline]
    pub fn pass<const SHIFT: i32>(
        w: &[i32; 8],
        large: bool,
        bias: __m256i,
        p02: __m256i,
        p46: __m256i,
        p13: __m256i,
        p57: __m256i,
    ) -> [(__m256i, __m256i); 4] {
        let add = |a, b| _mm256_add_epi32(a, b);
        let sub = |a, b| _mm256_sub_epi32(a, b);
        // Multiply the low halves by `lo` and the high halves by `hi`, add.
        let m = |p, lo: i32, hi: i32| {
            let pair = i32::from(hi as i16) << 16 | i32::from(lo as i16 as u16);
            _mm256_madd_epi16(p, _mm256_set1_epi32(pair))
        };

        let mut a0 = add(m(p02, w[4], w[2]), m(p46, w[4], w[6]));
        let mut a1 = sub(m(p02, w[4], w[6]), m(p46, w[4], w[2]));
        let mut a2 = sub(m(p02, w[4], -w[6]), m(p46, w[4], -w[2]));
        let mut a3 = add(m(p02, w[4], -w[2]), m(p46, w[4], -w[6]));
        let mut b0 = add(m(p13, w[1], w[3]), m(p57, w[5], w[7]));
        let mut b1 = sub(m(p13, w[3], -w[7]), m(p57, w[1], w[5]));
        let mut b2 = add(m(p13, w[5], -w[1]), m(p57, w[7], w[3]));
        let mut b3 = add(m(p13, w[7], -w[5]), m(p57, w[3], -w[1]));

        if large {
            // Constants 1, 2 and 3 are the large ones.
            let high = _mm256_set1_epi32(-65536);
            let x1 = _mm256_slli_epi32::<16>(p13);
            let x3 = _mm256_and_si256(p13, high);
            let x5 = _mm256_slli_epi32::<16>(p57);
            let x7 = _mm256_and_si256(p57, high);
            let x2 = _mm256_and_si256(p02, high);
            let x6 = _mm256_and_si256(p46, high);
            a0 = add(a0, x2);
            a1 = sub(a1, x6);
            a2 = add(a2, x6);
            a3 = sub(a3, x2);
            b0 = add(b0, add(x1, x3));
            b1 = add(b1, sub(x1, x5));
            b2 = add(b2, sub(x7, x3));
            b3 = add(b3, sub(x5, x7));
        }

        let out = |a, b| {
            let a = add(a, bias);
            (_mm256_srai_epi32::<SHIFT>(add(a, b)), _mm256_srai_epi32::<SHIFT>(sub(a, b)))
        };
        [out(a0, b0), out(a1, b1), out(a2, b2), out(a3, b3)]
    }

    /// From the packed results of the row pass to the input pairs of the
    /// column pass.
    ///
    /// `z[k]` holds results k and 7 - k of the row pass: in its low half
    /// rows 0..4 of k, then rows 0..4 of 7 - k; in its high half rows 4..8.
    /// The column pass wants, for each column, rows (0, 2), (4, 6), (1, 3)
    /// and (5, 7) side by side, columns in order.
    #[target_feature(enable = "avx2")]
    #[inline]
    pub fn regroup(z: [__m256i; 4]) -> (__m256i, __m256i, __m256i, __m256i) {
        // Rows 0 1 2 3 of a column become 0 2 1 3: pairs in 32-bit lanes.
        let order = _mm256_setr_epi8(
            0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15, //
            0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15,
        );
        let [z0, z1, z2, z3] = z.map(|v| _mm256_shuffle_epi8(v, order));
        // Columns 0..4 are the first two lanes of each half of z0..z3,
        // columns 4..8 the last two lanes of z3..z0.
        let a = _mm256_unpacklo_epi32(z0, z1);
        let b = _mm256_unpacklo_epi32(z2, z3);
        let c = _mm256_unpackhi_epi32(z3, z2);
        let d = _mm256_unpackhi_epi32(z1, z0);
        let u02 = _mm256_unpacklo_epi64(a, b);
        let u13 = _mm256_unpackhi_epi64(a, b);
        let v02 = _mm256_unpacklo_epi64(c, d);
        let v13 = _mm256_unpackhi_epi64(c, d);
        (
            _mm256_permute2x128_si256::<0x20>(u02, v02),
            _mm256_permute2x128_si256::<0x31>(u02, v02),
            _mm256_permute2x128_si256::<0x20>(u13, v13),
            _mm256_permute2x128_si256::<0x31>(u13, v13),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block given in natural order, with the same quantiser everywhere.
    fn run(kind: Idct, natural: &[i16; 64], q: i16) -> [[u16; 8]; 8] {
        let mut b = [0i16; 64];
        for (n, v) in natural.iter().enumerate() {
            b[usize::from(PERM[n])] = *v;
        }
        idct_block(kind, &b, &[q; 64])
    }

    fn flat(kind: Idct, dc: i16) -> [[u16; 8]; 8] {
        let mut b = [0i16; 64];
        b[0] = dc;
        run(kind, &b, 1)
    }

    #[test]
    fn storage_order_is_a_permutation() {
        let mut seen = [false; 64];
        for p in PERM {
            assert!(!seen[usize::from(p)]);
            seen[usize::from(p)] = true;
        }
        assert_eq!(PERM[0], 0);
        // Row 3: columns 0 and 2 side by side, then 4 and 6 a run further.
        assert_eq!([PERM[24], PERM[26], PERM[28], PERM[30]], [6, 7, 22, 23]);
        assert_eq!([PERM[25], PERM[27], PERM[29], PERM[31]], [38, 39, 54, 55]);
    }

    #[test]
    fn zero_block_is_mid_grey() {
        assert_eq!(flat(Idct::C10, 0), [[512; 8]; 8]);
        assert_eq!(flat(Idct::Asm10, 0), [[512; 8]; 8]);
        assert_eq!(flat(Idct::C12, 0), [[2048; 8]; 8]);
    }

    #[test]
    fn dc_scales_and_clips() {
        assert_eq!(flat(Idct::Asm10, 64), [[514; 8]; 8]);
        assert_eq!(flat(Idct::C10, 64), [[514; 8]; 8]);
        assert_eq!(flat(Idct::Asm10, 32000), [[1019; 8]; 8]);
        assert_eq!(flat(Idct::Asm10, -32000), [[4; 8]; 8]);
        assert_eq!(flat(Idct::C12, 64), [[2056; 8]; 8]);
        assert_eq!(flat(Idct::C12, 4000), [[2548; 8]; 8]);
        assert_eq!(flat(Idct::C12, 32000), [[4091; 8]; 8]);
    }

    #[test]
    fn a_horizontal_and_a_vertical_wave() {
        // The first AC coefficient of row 0 varies along x only.
        let mut b = [0i16; 64];
        b[1] = 800;
        let out = run(Idct::Asm10, &b, 1);
        assert!(out.iter().all(|r| *r == out[0]));
        assert!(out[0][0] > 512 && out[0][7] < 512);
        // The first coefficient of row 1 varies along y only.
        let mut b = [0i16; 64];
        b[8] = 800;
        let out = run(Idct::Asm10, &b, 1);
        assert!(out.iter().all(|r| r.iter().all(|&s| s == r[0])));
        assert!(out[0][0] > 512 && out[7][0] < 512);
    }

    /// The assembly and the C code agree on ordinary input...
    #[test]
    fn c10_and_asm10_agree_on_small_coefficients() {
        let mut seed = 99u32;
        for _ in 0..2000 {
            let mut b = [0i16; 64];
            for v in b.iter_mut() {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = ((seed >> 16) % 1024) as i16 - 512;
            }
            assert_eq!(run(Idct::C10, &b, 4), run(Idct::Asm10, &b, 4));
        }
    }

    /// ...and not on extreme input: the assembly's rounding term wraps
    /// around in 16 bits, and its row results saturate.
    #[test]
    fn c10_and_asm10_differ_at_the_extremes() {
        let mut b = [0i16; 64];
        b[0] = 32767;
        assert_eq!(run(Idct::C10, &b, 1), [[1019; 8]; 8]);
        assert_eq!(run(Idct::Asm10, &b, 1), [[4; 8]; 8]);

        let mut b = [0i16; 64];
        b[8..16].copy_from_slice(&[30000, 30000, 30000, 0, 0, 0, 0, 0]);
        assert_ne!(run(Idct::C10, &b, 1), run(Idct::Asm10, &b, 1));
    }
}
